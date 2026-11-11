//! The session registry: one live agent session, its harness, and its tailer.
//!
//! # The central design decision: two sources, one truth
//!
//! A driver's stdout and the harness's transcript file describe the same
//! session, but they are good at different things:
//!
//! | | stdout | transcript file |
//! |---|---|---|
//! | token deltas | yes | no |
//! | approvals, interrupt, turn results | yes | no |
//! | exactly what `agit commit` will store | **no** | **yes** |
//!
//! So we use both, for different purposes, and never let them compete:
//!
//! * stdout drives **ephemeral** events — `item.started`, `item.delta`,
//!   `turn.started/completed`, `approval.request`. None of it is persisted.
//! * the file drives **`item.completed`**, the only event the hub stores and the
//!   web renders permanently. Each new line is parsed with the very same
//!   `adapter::parse` that `agit show` uses, and carries its
//!   `transcript::object_hash` — the identical hash the committed envelope will
//!   have. That is what lets the hub reconcile its live projection against the
//!   pushed history line by line, and why "we talked about it on the web but
//!   `agit log` doesn't have it" is structurally impossible.
//!
//! # Nothing on this path is held back
//!
//! A supervised session is its owner's own, shown live to them or to colleagues
//! in their workspace, so its transcript records, streamed deltas and approval
//! cards go out as the harness produced them (see
//! `Redactor::for_device_control`). What is kept where others can read it
//! leaves through `agit save`, turn-end settlement or push, each of which runs
//! the full privacy check of its own.

pub(crate) mod archive_recovery;
mod background;
mod landing;
mod local_publication;
pub(crate) mod native_records;
mod settlement_io;
mod steer_queue;

use crate::domain::privacy_receipt::{PublicationReceipt, SupervisorPushRequest};
use crate::domain::{redact, transcript};
use crate::protocol::{
    ApprovalResponse, CommitSettled, Delivery, Frame, ItemCompleted, ItemDelta, ItemStarted,
    PermissionApply, PermissionMode, RAW_LINE_CAP, SecretDetected, SessionInfo,
    SessionPermissionMode, SessionStatus, TurnCompleted, TurnOutcome as PTurnOutcome, TurnSource,
    TurnStarted, method,
};
use crate::rc::harness::{
    AnyDriver, ApprovalOutcome, BoundedTurnIds, HarnessEvent, InterruptDispatch, LaunchSpec,
    PermissionModeChangeError, PermissionModeOutcome, SteerDispatch, TurnGuardAttempt, TurnOutcome,
    TurnStartDispatch, TurnStartOutcome,
};
use crate::rc::tail::Tailer;
use crate::rc::ticket::Ticket;
use std::path::PathBuf;

/// Whether a native source attached in the mode its launch asked for. A shared native thread
/// keeps the mode it is set to and the launch cannot change it, so the daemon admits a resume
/// only for the mode it found there, and an attach that opened in another mode, or in one it
/// cannot confirm, is refused rather than driven: it may have loosened since. A launch that asked
/// for no mode, which only the owner's resume of an unknown native mode does, asks nothing here.
pub(crate) fn opened_as_asked(
    asked: Option<PermissionMode>,
    opened: Option<PermissionMode>,
) -> bool {
    asked.is_none_or(|asked| opened == Some(asked))
}

/// Record the mode `driver` opened in, and whether the session is dangerous for it. A driver
/// that cannot know its mode, as a Claude Code terminal attached for native control cannot,
/// records none and counts as dangerous: nothing shows the terminal ran with checks, so only its
/// owner may drive it. The bit only ever goes from false to true here.
pub(crate) fn record_opening_mode(driver: &AnyDriver, info: &mut SessionInfo) {
    info.permission_mode = driver
        .permission_mode_known()
        .then(|| driver.permission_mode());
    info.dangerous |= info.permission_mode.is_none_or(|mode| mode.is_dangerous());
}
use std::sync::Arc;

use tokio::sync::mpsc;

/// Current authorization to perform RC repository settlement.
///
/// `epoch` changes on every connect/disconnect negotiation transition. A
/// settlement captures one epoch and must keep it through land, commit, push,
/// and notification; losing and quickly regaining the feature cannot revive an
/// in-flight operation that was authorized by the previous connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettlementState {
    /// A same-user local daemon owns settlement without a network lease.
    pub local_owner: bool,
    pub epoch: u64,
    pub agent_identity_v1: bool,
    /// Kept in the same connection-epoch state even though the supervisor does
    /// not consume it: daemon dispatch must make both negotiated authorities
    /// disappear atomically on disconnect/re-registration.
    pub session_start_idempotency_v1: bool,
}

fn settlement_lease(
    state: &tokio::sync::watch::Receiver<SettlementState>,
) -> Option<SettlementState> {
    let state = *state.borrow();
    (state.agent_identity_v1 || state.local_owner).then_some(state)
}

fn settlement_lease_is_current(
    state: &tokio::sync::watch::Receiver<SettlementState>,
    lease: SettlementState,
) -> bool {
    *state.borrow() == lease && (lease.agent_identity_v1 || lease.local_owner)
}

const SETTLEMENT_DELIVERY_WAIT: std::time::Duration = std::time::Duration::from_secs(12);

async fn wait_for_connection_delivery_within(
    state: &mut tokio::sync::watch::Receiver<SettlementState>,
    lease: SettlementState,
    delivery: &Arc<crate::protocol::ConnectionDelivery>,
    within: std::time::Duration,
) -> crate::protocol::DeliveryStatus {
    let wait = async {
        loop {
            let status = delivery.status();
            if status != crate::protocol::DeliveryStatus::Pending {
                return status;
            }
            if !settlement_lease_is_current(state, lease) {
                delivery.invalidate();
                return delivery.status();
            }
            tokio::select! {
                changed = state.changed() => {
                    if changed.is_err() || !settlement_lease_is_current(state, lease) {
                        // A websocket write may have completed at the same instant
                        // as the disconnect. Delivered wins; invalidate is a CAS
                        // from Pending and therefore cannot erase that evidence.
                        delivery.invalidate();
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
        }
    };
    tokio::time::timeout(within, wait)
        .await
        .unwrap_or(crate::protocol::DeliveryStatus::Pending)
}

struct PendingSettlement {
    sha: String,
    delivery: Option<Arc<crate::protocol::ConnectionDelivery>>,
    /// The **durable** receipt for this settlement, see [`unacked_settlement_path`]. Deleted
    /// only once the hub confirms it took `commit.settled`; it outlives the process.
    receipt: Option<PathBuf>,
}

enum PublicationOutcome {
    Complete(Option<Arc<crate::protocol::ConnectionDelivery>>),
    Retry,
}

struct Publication(tokio::task::JoinHandle<PublicationOutcome>);

#[derive(Default)]
struct PublicationRetry {
    failures: u32,
    next: Option<tokio::time::Instant>,
}

impl Session {
    /// After a settlement that settled no new turn. One that stopped at a privacy wait leaves its
    /// turn unsettled, so the idle retry keeps running until the turn lands: the wait ends by
    /// itself when the worker was only slow or busy, and when the person ends its cause (their
    /// rules readable again), and nothing else settles the turn before the next one ends. A
    /// record refused for its size meets the same refusal on every try, so it doesn't retry. A
    /// settlement that reset the timer here would leave such a turn unsaved until another turn or
    /// the session's stop, however long that is.
    fn after_settling_nothing(&mut self, wait: Option<&crate::domain::privacy::pending::Waiting>) {
        match wait.filter(|wait| wait.refused_bytes.is_none()) {
            Some(wait) if crate::domain::privacy::service::transient(&wait.reason) => {
                self.publication_retry.failed();
            }
            Some(_) => self.publication_retry.refused(),
            None => self.publication_retry = PublicationRetry::default(),
        }
    }

    /// Where a settlement or a publication would leave nothing more to do: the retry stops only
    /// if no privacy wait is recorded for this conversation. A publication that delivered an
    /// earlier turn says nothing about a later one whose settlement stopped at the check, so
    /// clearing the timer there would strand that turn as surely as a settlement that settled
    /// nothing.
    fn retry_while_a_turn_waits(&mut self) {
        let wait = self
            .privacy_claim
            .as_ref()
            .and_then(|claim| crate::domain::privacy::pending::load().remove(claim));
        self.after_settling_nothing(wait.as_ref());
    }
}

impl PublicationRetry {
    fn failed(&mut self) {
        self.schedule(300);
    }

    /// A policy refusal changes only when the transcript or local configuration does; each
    /// idle attempt rereads the whole lineage, so it backs off further.
    fn refused(&mut self) {
        self.schedule(30 * 60);
    }

    fn schedule(&mut self, cap_seconds: u64) {
        self.failures = self.failures.saturating_add(1);
        let seconds = 2_u64.saturating_pow(self.failures.min(12)).min(cap_seconds);
        self.next = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(seconds));
    }

    fn due(&self) -> bool {
        self.next
            .is_some_and(|next| tokio::time::Instant::now() >= next)
    }
}

impl Drop for Publication {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[allow(clippy::too_many_arguments)]
async fn publish_settlement(
    mut state: tokio::sync::watch::Receiver<SettlementState>,
    lease: SettlementState,
    command: tokio::process::Command,
    request: SupervisorPushRequest,
    result_file: tempfile::NamedTempFile,
    mut notification: Frame,
    out: mpsc::Sender<Frame>,
    record: crate::Result<crate::commands::upload_outcome::Place>,
) -> Option<Arc<crate::protocol::ConnectionDelivery>> {
    use crate::commands::upload_outcome::{Refusal, recorded_async};
    // The publication goes through only with the Hub's receipt for this very request.
    let recorded = recorded_async(
        record,
        request.source.clone(),
        async {
            let push = guarded_output(&mut state, lease, command).await?;
            let published = confirmed_strict_push(&push, &request, result_file.path());
            Some((push, published))
        },
        |pushed| match pushed {
            Some((_, Some(_))) => None,
            Some((push, None)) => Some(Refusal::of(push)),
            None => Some(Refusal::unexplained()),
        },
    )
    .await;
    let (push, published) = match recorded {
        Ok(pushed) => pushed?,
        Err(unrecorded) => {
            tracing_note(&format!(
                "RC publication did not run; the pending commit remains eligible for retry: {unrecorded}"
            ));
            return None;
        }
    };
    let Some(published) = published else {
        tracing_note(&format!(
            "RC publication has no matching successful receipt; the pending commit remains eligible for retry: {}",
            String::from_utf8_lossy(&push.stderr).trim()
        ));
        return None;
    };
    if !settlement_lease_is_current(&state, lease) {
        return None;
    }
    let delivery = crate::protocol::ConnectionDelivery::new(
        lease.epoch,
        crate::protocol::ConnectionFeature::AgentIdentityV1,
    );
    notification.params.as_mut()?["commit_sha"] = serde_json::Value::String(published);
    notification.connection_delivery = Some(delivery.clone());
    if out.send(notification).await.is_err() {
        delivery.invalidate();
        return None;
    }
    wait_for_connection_delivery_within(&mut state, lease, &delivery, SETTLEMENT_DELIVERY_WAIT)
        .await;
    Some(delivery)
}

/// Notification delivery has its own durable source watermark: publication acknowledgement
/// cannot prove that `commit.settled` arrived. Write it before publication and remove it only
/// on confirmed delivery so a restart can retry either unfinished step.
///
/// Branch names are hashed to support path separators and platform-specific filename rules.
/// Each repository retains these receipts locally, independently of the published Git tree.
fn unacked_settlement_path(repo_dir: &std::path::Path, branch: &str) -> PathBuf {
    use sha2::Digest as _;
    let key = hex::encode(sha2::Sha256::digest(branch.as_bytes()));
    repo_dir
        .join(".git")
        .join("agit-rc-unacked")
        .join(&key[..32])
}

/// Write the receipt. Failure does not interrupt the settlement — without a receipt the most
/// that is lost is one notification, while refusing to push because a bookkeeping file would not
/// write loses the whole turn.
fn record_unacked_settlement(path: &std::path::Path, sha: &str, branch: &str) {
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        tracing_note("could not create the RC settlement receipt directory");
        return;
    }
    // The second line is for people only: the digest does not yield the branch name back, and
    // whoever is debugging needs to know which branch this receipt belongs to.
    if std::fs::write(path, format!("{sha}\n{branch}\n")).is_err() {
        tracing_note("could not record the unacked RC settlement receipt");
    }
}

/// Read the sha back out of a receipt.
fn read_unacked_settlement(path: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let sha = text.lines().next().unwrap_or_default().trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Where a settlement subprocess comes from. **Only tests replace it; in production it is
/// always `None`.**
///
/// In production the executable is `current_exe()` (the daemon itself: `commit
/// --from-supervisor`, `push` and `rc land` are private subcommands of the same binary), and the
/// repo directory is computed by lineage from `$AGIT_HOME`. A unit test can reach neither —
/// `current_exe()` is libtest, and `$AGIT_HOME` is **process-wide**: the real one writes into
/// the user's own `~/.agit`, and pointing it at a temporary directory upends the other tests
/// running in parallel (see the comment on `rc::with_agit_home`).
///
/// Without this seam, not one line of the chain "the subprocess really wrote the result file,
/// and `commit.settled` really left `self.out` carrying that sha" is covered. And when that
/// whole chain is dead — `guarded_output` without `Stdio::piped()` — every unit test is green.
struct SettlementChild {
    exe: PathBuf,
    repo_dir: PathBuf,
}

#[derive(Clone)]
struct SettlementProgram {
    exe: PathBuf,
    #[cfg(all(test, unix))]
    script: bool,
}

impl SettlementProgram {
    fn command(&self) -> tokio::process::Command {
        // Fixture scripts are input to the interpreter: writable script handles must not
        // prevent settlement or landing from starting under concurrent test execution.
        #[cfg(all(test, unix))]
        if self.script {
            let mut command = tokio::process::Command::new("/bin/sh");
            command.arg(&self.exe);
            return command;
        }
        let mut command = tokio::process::Command::new(&self.exe);
        command.env(
            crate::infra::own_image::PARENT_BUILD,
            crate::infra::own_image::BUILD,
        );
        command
    }
}

/// Which boundary this settlement runs on.
///
/// The only difference is whether there is a next time after a failure: yielding is right on a
/// turn boundary, and yielding on the exit boundary drops the final turn.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettlementBoundary {
    /// A turn ended. What this attempt does not finish, the next turn comes back for.
    Turn,
    /// The session is closing. **There is no next time.**
    SessionExit,
}

/// Run one settlement subprocess under a connection-scoped feature lease.
/// Any negotiation transition cancels the whole process group. The epoch check
/// means a fast disconnect/reconnect cannot revive an operation from the old
/// connection even if both endpoints support the feature.
pub(in crate::rc) async fn guarded_output(
    state: &mut tokio::sync::watch::Receiver<SettlementState>,
    lease: SettlementState,
    mut command: tokio::process::Command,
) -> Option<std::process::Output> {
    if !settlement_lease_is_current(state, lease) {
        return None;
    }
    // On Unix the group guard below ends the child with the rest of its group. A kill on drop
    // would SIGKILL the child first, a Git among them, before it can remove its locks.
    command.kill_on_drop(cfg!(windows));
    // **stdout/stderr must be taken over explicitly.**
    //
    // tokio's `Command` **inherits** the parent's stdio by default, and `wait_with_output()`
    // only collects the two pipes it owns itself. Without piped, the child's output flows
    // straight into the daemon's own log and the `Output.stdout` returned here is always the
    // empty string — while `status` says success.
    //
    // That lands on the worst shape: `read_head` reads `git rev-parse HEAD` as an empty string,
    // `strict_settlement_candidate` calls it "strict commit left an unreadable HEAD", and so
    // **every RC settlement commits locally and never reports `commit.settled`**. On the web
    // that conversation looks like it was never persisted.
    //
    // A unit test cannot see this — it exercises the test in `strict_settlement_candidate`, and
    // that test is right; what is wrong is the empty string fed to it.
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    let job = crate::rc::windows_job::Job::new().ok()?;
    #[cfg(windows)]
    crate::rc::windows_job::Job::configure(&mut command);
    let child = guarded_spawn(state, lease, &mut command).await?;

    #[cfg(windows)]
    let child = {
        let mut child = child;
        if let Err(error) = job.attach_and_resume(&child) {
            // The process was born suspended, so assignment failure cannot leave
            // an already-running remote helper outside the Job. Cover both cases:
            // TerminateJobObject for an assigned process, direct kill otherwise.
            eprintln!("agitd: could not fence a settlement process tree: {error}");
            let _ = job.terminate();
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = job.wait_empty().await;
            return None;
        }
        child
    };

    #[cfg(unix)]
    let pgid = child.id().map(|id| id as i32);
    let mut wait = Box::pin(child.wait_with_output());
    // Declared after `wait`, so a dropped operation ends the group before the child is let go.
    #[cfg(unix)]
    let mut group = ProcessGroupGuard(pgid);
    loop {
        tokio::select! {
            biased;
            changed = state.changed() => {
                if changed.is_err() || !settlement_lease_is_current(state, lease) {
                    // A lease that ended fences the operation: it is ended, never let finish.
                    #[cfg(unix)]
                    {
                        // The guard stays armed through the wait: if this operation is dropped
                        // during it, what ignores SIGTERM still gets the SIGKILL.
                        match group.0 {
                            Some(pgid) => {
                                end_group(pgid, &mut wait).await;
                                group.disarm();
                            }
                            None => {
                                let _ = (&mut wait).await;
                            }
                        }
                    }
                    #[cfg(windows)]
                    {
                        // Keep both owners alive until the direct child is
                        // reaped and Job accounting proves every descendant is
                        // gone. Returning immediately would recreate the old
                        // direct-child-only window through Drop ordering.
                        if job.terminate().is_ok() {
                            let _ = (&mut wait).await;
                            drop(wait);
                            let _ = job.wait_empty().await;
                        }
                    }
                    return None;
                }
            }
            output = &mut wait => {
                #[cfg(unix)]
                group.disarm();
                let output = output.ok()?;
                drop(wait);
                #[cfg(windows)]
                {
                    // A command can exit while a detached remote helper is
                    // still in the Job. Do not release the lease guard or
                    // report success until the whole tree is empty; an ACK
                    // transition while waiting terminates the descendants.
                    loop {
                        if !settlement_lease_is_current(state, lease) {
                            let _ = job.terminate();
                            let _ = job.wait_empty().await;
                            return None;
                        }
                        match job.active_processes() {
                            Ok(0) => break,
                            Ok(_) => {}
                            Err(error) => {
                                eprintln!("agitd: could not verify settlement process-tree exit: {error}");
                                let _ = job.terminate();
                                let _ = job.wait_empty().await;
                                return None;
                            }
                        }
                        tokio::select! {
                            changed = state.changed() => {
                                if changed.is_err() || !settlement_lease_is_current(state, lease) {
                                    let _ = job.terminate();
                                    let _ = job.wait_empty().await;
                                    return None;
                                }
                            }
                            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
                        }
                    }
                }
                return settlement_lease_is_current(state, lease).then_some(output);
            }
        }
    }
}

fn executable_is_busy(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::ETXTBSY)
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

/// An executable held open for writing has not started when exec rejects it.
/// Retrying that refusal is bounded and requires the same current authority lease.
async fn guarded_spawn(
    state: &mut tokio::sync::watch::Receiver<SettlementState>,
    lease: SettlementState,
    command: &mut tokio::process::Command,
) -> Option<tokio::process::Child> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        if !settlement_lease_is_current(state, lease) {
            return None;
        }
        match command.spawn() {
            Ok(child) => return Some(child),
            Err(error) if executable_is_busy(&error) && tokio::time::Instant::now() < deadline => {
                tokio::select! {
                    biased;
                    changed = state.changed() => {
                        if changed.is_err() || !settlement_lease_is_current(state, lease) {
                            return None;
                        }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
                }
            }
            Err(error) => {
                tracing_note(&format!("could not start settlement subprocess: {error}"));
                return None;
            }
        }
    }
}

/// The ref the settlement watermark reads: the session branch itself.
///
/// The main checkout stays on `main` and every session branch settles in its own worktree; with
/// the main checkout's HEAD as the watermark, a successful settlement reads as "HEAD did not
/// move" and never gets pushed.
pub(crate) fn settlement_watermark_ref(branch: &str) -> String {
    format!("refs/heads/{branch}")
}

/// Decide whether a successful strict commit covers a new (or previously
/// committed-but-not-yet-pushed) settlement. Exit status alone is not enough:
/// the ordinary Stop-hook entry point intentionally returns zero for several
/// no-op/error cases, and pushing in those cases would attribute the current
/// journal cursor to an older HEAD.
fn strict_settlement_candidate(
    before: &str,
    commit: &std::process::Output,
    after: &str,
    reported: Option<&str>,
    pending: Option<&str>,
) -> Result<Option<String>, &'static str> {
    if !commit.status.success() {
        return Err("strict commit failed");
    }
    if after.is_empty() {
        return Err("strict commit left an unreadable HEAD");
    }
    if reported == Some(crate::commands::commit::archive::WITHHELD_RESULT) {
        return if after == before {
            Ok(None)
        } else {
            Err("withheld archive exploration changed HEAD")
        };
    }
    if let Some(reported) = reported {
        if reported != after || reported == before {
            return Err("strict commit result does not name the newly published HEAD");
        }
        return Ok(Some(reported.to_string()));
    }
    if after != before {
        return Err("HEAD changed without a strict transcript settlement result");
    }
    match pending {
        Some(sha) if sha == after => Ok(Some(sha.to_string())),
        Some(_) => Err("HEAD moved away from the pending settlement"),
        None => Ok(None),
    }
}

fn confirmed_strict_push(
    push: &std::process::Output,
    request: &SupervisorPushRequest,
    path: &std::path::Path,
) -> Option<String> {
    push.status
        .success()
        .then(|| request.read_result(path).ok())
        .flatten()
        .map(|receipt| receipt.published)
}

/// A matching local publication receipt accounts for a private source commit even when
/// the remote contains only its generated public counterpart. Without that evidence,
/// reachability excludes remote history and untouched local main baselines from retries.
/// The glob also works when no main ref exists; an identity or receipt error prevents
/// deriving a settlement for an unverified destination.
async fn unpushed_local_head(
    state: &mut tokio::sync::watch::Receiver<SettlementState>,
    lease: SettlementState,
    repo_dir: &str,
    head: &str,
    branch: &str,
    expected_agent_id: &str,
) -> Option<String> {
    if !settlement_lease_is_current(state, lease) {
        return None;
    }
    if !lease.local_owner {
        let repo = crate::domain::repo::Repo::at(std::path::Path::new(repo_dir));
        let confirmed = (|| -> crate::Result<bool> {
            let identity =
                crate::hub::identity::require_current(&repo, &crate::infra::config::hub_url())?;
            anyhow::ensure!(
                identity.agent_id == expected_agent_id,
                "RC publication destination changed"
            );
            PublicationReceipt::load(&repo, branch)?
                .map(|receipt| receipt.matches(&repo, branch, head, &identity))
                .transpose()
                .map(|value| value.unwrap_or(false))
        })();
        match confirmed {
            Ok(true) => return None,
            Ok(false) => {}
            Err(error) => {
                tracing_note(&format!("cannot verify RC publication history: {error:#}"));
                return None;
            }
        }
    }
    let remotes = format!("--remotes={}", crate::domain::repo::ORIGIN);
    let mut rev = crate::infra::git_runtime::async_command();
    rev.args(crate::domain::meta::GIT_SAFE)
        .args(["-C", repo_dir, "rev-list", "--max-count=1", head, "--not"])
        .arg(&remotes)
        .arg("--glob=refs/heads/main*")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    let out = guarded_output(state, lease, rev).await?;
    if !out.status.success() {
        // An unanswerable question means no pending commit: better to skip a push than to let
        // one git failure push a commit whose provenance cannot be explained.
        return None;
    }
    let unreachable = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!unreachable.is_empty()).then(|| head.to_string())
}

/// What a `commit.failed` event says about a settlement that stopped. The event can leave this
/// computer, and a Git lock's path names the repository and the session branch, so a failure that
/// names one is told by its class and where to find the file.
fn settlement_failure_detail(reason: &str, stderr: &str) -> String {
    if stderr.contains("index.lock") {
        return format!(
            "{reason}: a stale checkout lock on this computer keeps this conversation's archive from finishing; `agit doctor` names the file"
        );
    }
    format!("{reason}: {}", stderr.trim())
}

/// How long a settlement subprocess's group gets to exit after SIGTERM before SIGKILL. Git
/// removes the lock files it holds when it is terminated, not when it is killed, and an index lock
/// left in a checkout refuses every later attempt at that conversation.
#[cfg(unix)]
const TERMINATE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether any process of group `pgid` is left.
#[cfg(unix)]
fn group_alive(pgid: i32) -> bool {
    unsafe { libc::kill(-pgid, 0) == 0 }
}

/// Ends group `pgid`, whose leader `wait` reaps, and polls `wait` to completion exactly once:
/// SIGTERM first, so Git removes its locks, and SIGKILL for whatever outlives
/// [`TERMINATE_GRACE`]. The group is signalled again only while a member is left, so a group id
/// the system reuses afterwards is never touched.
#[cfg(unix)]
async fn end_group<F: std::future::Future + Unpin>(pgid: i32, wait: &mut F) {
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }
    let deadline = tokio::time::Instant::now() + TERMINATE_GRACE;
    let reaped = tokio::time::timeout_at(deadline, &mut *wait).await.is_ok();
    while group_alive(pgid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    if group_alive(pgid) {
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
    if !reaped {
        let _ = wait.await;
    }
}

/// `kill_on_drop` stops the direct child. Settlement commands can be waiting on
/// git grandchildren, so on Unix the lease also owns the process group, and ends it as
/// [`end_group`] does when the operation is dropped. Windows uses a suspended-at-birth
/// kill-on-close Job Object in [`guarded_output`].
#[cfg(unix)]
struct ProcessGroupGuard(Option<i32>);

#[cfg(unix)]
impl ProcessGroupGuard {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            // Negative pid targets the process group created above. ESRCH is
            // expected when the child won the race and already exited. Nothing can wait here, so
            // the SIGKILL for a member that outlives the grace comes from a thread of its own.
            unsafe {
                libc::kill(-pgid, libc::SIGTERM);
            }
            std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + TERMINATE_GRACE;
                while group_alive(pgid) && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                if group_alive(pgid) {
                    unsafe {
                        libc::kill(-pgid, libc::SIGKILL);
                    }
                }
            });
        }
    }
}

/// How often to poll the transcript file. The harness appends every few hundred
/// ms at most, so a poll this short is imperceptible and, unlike inotify,
/// behaves the same on every platform and when the file is *replaced* (which
/// `agit resume`'s slow path does).
pub const TAIL_POLL_MS: u64 = 120;
const ROLLOVER_CHECK: std::time::Duration = std::time::Duration::from_secs(2);
/// How long a stop waits for the turn it interrupted to end. It sits inside the daemon's own exit
/// bound with room for the runtime's teardown and the exit settlement after it.
const STOP_TURN_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

/// How long, in milliseconds, to wait at most for the transcript file to go quiet before
/// declaring a turn over.
pub const SETTLE_MAX_MS: u64 = 1500;

/// What answering a pending approval needs to know. See `Session::pending`.
struct PendingApproval {
    /// A session-scoped Claude approval changes the global driver mode. Keep
    /// the machine-originated prediction beside the authorization evidence so
    /// the execution side can require a durable danger permit before echoing a
    /// bypass suggestion.
    suggested_permission_mode: Option<PermissionMode>,
}

#[derive(Debug, Clone, Default)]
pub struct MessageAttribution {
    pub by: Option<String>,
    pub sender: Option<crate::protocol::MessageSender>,
    pub client_msg_id: Option<String>,
    pub native_prompt_id: Option<String>,
    /// Whether these are the owner's words, as the device sees the caller. False by default, so
    /// words of unknown origin are never taken for the owner's.
    pub owner: bool,
}

impl MessageAttribution {
    fn same_sender(&self, other: &Self) -> bool {
        match (&self.sender, &other.sender) {
            (Some(left), Some(right)) => left.account_id == right.account_id,
            (None, None) => self
                .by
                .as_ref()
                .zip(other.by.as_ref())
                .is_some_and(|(left, right)| !left.is_empty() && left == right),
            _ => false,
        }
    }

    pub(crate) fn from_caller(
        caller: &crate::protocol::CallerClaim,
        legacy_by: Option<String>,
        client_msg_id: Option<String>,
    ) -> Self {
        let sender = caller
            .account_id
            .as_ref()
            .filter(|account_id| !account_id.is_empty())
            .map(|account_id| crate::protocol::MessageSender {
                account_id: account_id.clone(),
                username: caller.username.clone().unwrap_or_default(),
            });
        Self {
            by: caller.username.clone().or(legacy_by),
            sender,
            client_msg_id,
            native_prompt_id: None,
            owner: caller.is_owner(),
        }
    }
}

#[derive(Debug)]
struct InitialTurn {
    message: String,
    attribution: MessageAttribution,
}

struct PendingTurnCommand {
    message: String,
    attribution: MessageAttribution,
    /// `None` is the creation-time fire-and-forget prompt. Viewer-originated
    /// turns always retain their ticket until the exact native outcome.
    reply: Option<Ticket<TurnStartOutcome>>,
    initial: bool,
    guard_attempt: Option<crate::rc::harness::TurnGuardAttempt>,
}

struct PendingSteerCommand {
    message: String,
    attribution: MessageAttribution,
    reply: Ticket<Delivery>,
}

enum PendingInitialReply {
    Attached,
    Blocked(Ticket<TurnStartOutcome>),
    Absent(Ticket<TurnStartOutcome>),
}

#[derive(Clone)]
struct ResolvedInitialTurn {
    message: String,
    attribution: MessageAttribution,
    outcome: TurnStartOutcome,
}

struct PreparedTurnStarted {
    frame: TurnStarted,
    mark_running: bool,
    /// Device-local rule hits inside this prompt. The reservation is synchronous and the alert
    /// needs `.await`, so the hits travel with the reservation result and go out at
    /// [`Session::publish_turn_started`].
    registered_ids: Vec<String>,
}

#[derive(Clone)]
enum TurnGuardBarrier {
    NativeSettings { mode: Option<PermissionMode> },
    Ready,
    Observe { confirmation_token: String },
    Confirm { confirmation_token: String },
    FailClosed { confirmation_token: String },
}

/// A live session under supervision.
pub struct Session {
    pub info: SessionInfo,
    driver: AnyDriver,
    /// The session was running a turn when it ended: the writer may have written up to its end.
    stopped_in_turn: bool,
    background_poll_at: tokio::time::Instant,
    transcript_readable: bool,
    tailer: Option<Tailer>,
    /// When the heartbeat next re-resolves the transcript to follow a Codex segment rollover.
    rollover_check_at: tokio::time::Instant,
    native_records: native_records::NativeRecords,
    redactor: redact::Redactor,
    /// Emitted upward; the daemon numbers these and ships them to the hub.
    out: mpsc::Sender<Frame>,
    /// An approval already sent and not yet answered, with what answering it needs.
    pending: std::collections::HashMap<String, PendingApproval>,
    /// The **byte** position consumed from the transcript. Used only to decide whether the file
    /// is still growing.
    ///
    /// Of the available coordinates, only this one cannot be fooled:
    ///
    /// * item count — a batch of new lines can be filtered by the adapter down to no items at
    ///   all (summary lines, meta lines);
    /// * line count — while the transcript is **writing the last record in pieces** there is no
    ///   newline yet, so `poll()` keeps those bytes in `pending` and returns an empty list.
    ///
    /// Either one lets `settle_transcript` decide the file went quiet while it is plainly still
    /// growing, so the turn is declared over early and the last lines fall outside the
    /// settlement.
    consumed_bytes: u64,
    /// Whether this run **continues** an existing transcript or opens a new one.
    ///
    /// It decides whether the tailer reads from the start or seeks to the end. Fixed at
    /// `launch` — see the comment there: "does the file exist right now" is a time-sensitive
    /// observation, and it would be asked after a landing that can take seconds.
    resuming: bool,
    /// Where this session settles: `owner/name@branch`, filled in by the hub at
    /// `session.start` / `session.resume`; without it there is nowhere to push. **A parsed
    /// form** — it becomes a local repo path, is injected into the harness as `AGIT_SESSION`,
    /// and is written into the roster, all of it coming from the hub. The test sits at the
    /// construction site, see [`crate::rc::lineage`].
    agit_session: Option<crate::rc::lineage::AgitSession>,
    /// Whether the local lineage (repo / main file line / branch / store link) is in place.
    ///
    /// It cannot all be done at launch: codex's thread id exists only after `thread/started`,
    /// and landing needs it. So this is a **retryable state transition** — a first attempt when
    /// `Ready` arrives, and another before every settlement while it has not succeeded. Failing
    /// means this session never settles into a commit, so better to retry every turn than to
    /// try once.
    /// **Which** harness id it has already landed under.
    ///
    /// Not a bool: claude's slow-path recovery mints a new session id at `system/init` (and a
    /// new transcript file with it), while the store link is recorded by id. Recording only
    /// whether it landed leaves the link pointing forever at the transcript nobody appends to,
    /// and every later turn settles nothing.
    landed_thread: Option<String>,
    landing: Option<landing::LandingTask>,
    /// Retained classification survives failed refreshes; only `landed_thread` grants fresh use.
    archive_handoff: Option<crate::commands::commit::archive::RcHandoff>,
    /// Tells the daemon the harness's own id. A separate channel rather than the event stream:
    /// this id never reaches the wire (viewers address only the logical id), but the daemon
    /// needs it for the double-writer guard and the roster.
    notes: mpsc::Sender<SessionNote>,
    /// The project directory. `agit commit` / `agit push` run here.
    cwd: PathBuf,
    /// Which generation of the same logical id this session is. Reported back with `Ended` on
    /// exit so the daemon removes only its own generation.
    generation: u64,
    pub(crate) publication_incarnation: Option<String>,
    /// This workspace's allowlist **as it stands now**, plus the command names the owner
    /// granted.
    ///
    /// **Not a snapshot.** A `Vec<PathBuf>` copied out of the mirror at `Session::launch` and
    /// then frozen for the session's lifetime goes stale, and the test rests entirely on this
    /// value: it **is** everything that makes "this is confined" true — see
    /// [`crate::rc::Confinement`].
    confinement: tokio::sync::watch::Receiver<crate::rc::Confinement>,
    /// Live feature ACK. This is a lease, not a startup snapshot: disconnect or
    /// renegotiation invalidates an in-flight settlement immediately.
    settlement: tokio::sync::watch::Receiver<SettlementState>,
    /// An unconfirmed source remains eligible for idempotent publication even when
    /// strict commit reports no new transcript content.
    pending_settlement: Option<PendingSettlement>,
    /// Publication owns the repository writer until its process tree has finished.
    publication: Option<Publication>,
    publication_retry: PublicationRetry,
    /// The claim this conversation's settlements record a privacy wait under, once one has run.
    privacy_claim: Option<String>,
    /// The settlement failure the client was last told about. The same failure recurs until
    /// the transcript or the branch changes, and one notice is enough.
    reported_settlement_failure: Option<String>,
    local_settlement: Option<settlement_io::LocalSettlement>,
    settlement_due: bool,
    /// The daemon told this session to stop, by its own exit or restart. That can be a person's stop,
    /// so a turn it cuts off is theirs; a harness that ends on its own stopped without anyone.
    stop_requested: bool,
    /// While a stop waits for the turn it interrupted to end: until when. The turn then ends the
    /// way a person's interrupt ends it, archived and announced, instead of being cut by the kill.
    stopping_turn: Option<tokio::time::Instant>,
    /// How long that wait may last: [`STOP_TURN_GRACE`] outside tests.
    stop_turn_grace: std::time::Duration,
    /// Nobody chose to stop the running turn ([`Command::ShutdownUnattended`]), and no person's
    /// interrupt was already on its way: the turn ends where agit stops it and is archived.
    stop_unattended: bool,
    completed_boundary: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// See [`SettlementChild`]: test-only, always `None` in production.
    settlement_child: Option<SettlementChild>,
    /// Creation may supply a prompt before Codex has a thread id. It is the
    /// only prompt allowed to wait for Ready without an RPC receipt.
    queued_initial_turn: Option<InitialTurn>,
    /// Metadata and optional viewer receipt for the one native `turn/start`
    /// currently awaiting its exact response.
    pending_turn_command: Option<PendingTurnCommand>,
    pending_steer_command: Option<PendingSteerCommand>,
    /// Steers headless Claude Code took for the running turn's next tool step, oldest first, with
    /// the native id their echo is named by: see [`Session::name_absorbed_steers`].
    absorbing_steers: std::collections::VecDeque<AbsorbingSteer>,
    /// How many turns agit has started on headless Claude Code: each one's number, which a
    /// waiting steer keeps for the turn it was written in.
    turns_started: u64,
    /// The ids agit gave the prompts of the most recent turns it started, with each turn's number,
    /// oldest first: a prompt read with a number past a steer's comes after that steer.
    started_prompts: std::collections::VecDeque<(String, u64)>,
    /// Messages for a running native Claude Code turn, held for its next tool step or waiting for
    /// the next turn; see [`steer_queue`].
    steer_queue: steer_queue::SteerQueue,
    pending_interrupt_command: Option<Ticket<()>>,
    /// The creation prompt has no original RPC receipt. Keep its authoritative
    /// result until the first viewer retry (or a different prompt) so a select
    /// race after native resolution cannot submit the same prompt twice.
    resolved_initial_turn: Option<ResolvedInitialTurn>,
    /// Synthetic heads from approval/completion evidence suppress a matching
    /// late native start instead of reopening the completed/approval state.
    /// Every native turn id announced during this harness generation. Native
    /// delivery has no bounded lateness, so silently evicting an old id would
    /// let a delayed duplicate create a second head and resurrect UI state.
    announced_turn_ids: BoundedTurnIds,
    /// One device-local rule alerts once per session; every occurrence is still redacted.
    alerted_registered: std::collections::HashSet<String>,
}

/// Internal notes from a session to the daemon.
///
/// Separate from [`Frame`]: none of this **reaches the wire**. `Bound` carries the harness's own
/// session id, which the protocol deliberately does not expose; the daemon needs it for two
/// things — recognizing that this local session is already supervised (so a second process does
/// not fight for the same transcript file), and writing it into the roster so it can be revived
/// by logical id after a restart.
pub enum SessionNote {
    /// Shared settings must reach the durable danger ledger before new input is handled.
    NativeSettings {
        session_id: String,
        generation: u64,
        mode: Option<PermissionMode>,
        ack: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// This session finished (the harness exited, the driver hit EOF, or Shutdown arrived).
    ///
    /// The daemon removes it from `sessions` on this note. Without it the row **stays forever**
    /// while the command channel it holds is dead: `session.resume` short-circuits on the
    /// already-supervised branch and hands back the stale `SessionInfo` (the web believes it is
    /// connected), the next `turn.start` gets a dead channel, and `rrx.await` answers `session
    /// went away` forever — with no way back, because `local_sessions` also drops this
    /// conversation from the adoptable list while that `runtime_thread_id` is still "alive". A
    /// finished session becomes unrecoverable.
    Ended {
        session_id: String,
        /// **Which generation** this note comes from.
        ///
        /// One logical id can be brought back up (a `session.resume` inserts a new row while
        /// the outgoing supervisor is still winding down). Without the generation, the `Ended`
        /// that generation sends on exit removes the **new** row from the table — the freshly
        /// started harness process is orphaned, and every operation the web performs on it
        /// answers "no live session". `WatchEnded` carries a generation for the same reason.
        generation: u64,
    },
    /// The harness process tree of this generation has been reaped: nothing it started can
    /// still write its conversation `thread`. Sent before settlement and publication, which may
    /// take long. It names the conversation itself because the daemon may have retired the
    /// session row before it reads this note; `in_turn` says the writer was stopped mid-turn.
    WriterReleased {
        session_id: String,
        generation: u64,
        thread: Option<String>,
        in_turn: bool,
    },
    /// This generation's harness cleanup failed: its process may still write `thread`, so the
    /// conversation stays with it. `process` is that process's pid and start time, read before
    /// the cleanup, so a daemon restart can tell whether it still runs.
    WriterNotReleased {
        session_id: String,
        generation: u64,
        thread: Option<String>,
        process: Option<(u32, i64)>,
    },
    /// A read-only follow ended on its own (the transcript is gone, or it stayed quiet too
    /// long). The daemon removes it from the table on this note — otherwise `watches` only
    /// grows.
    ///
    /// It carries the generation: while this note waits in the queue, the same key may already
    /// have been rebuilt by a new `session.watch`. See `daemon::WatchLive::generation`.
    WatchEnded { stream: String, generation: u64 },
    /// A terminal went away on its own (the shell exited, or the process was killed). The
    /// daemon removes it from the table on this note.
    ///
    /// `terminal.close` as the only reclamation path is not enough: that frame never arrives
    /// when the shell `exit`s on its own or the viewer simply closes the tab, and a `TermLive`
    /// left in the table holds a PTY master fd and an unreaped child process. One leak per open;
    /// the terminals on the machine only grow.
    TerminalExited { terminal_id: String },
    /// A freshly launched harness generation has reached its authoritative
    /// Ready boundary. The daemon may now replace inherited ambiguous guard
    /// attempts with a durable Plan baseline; attempts armed by this same live
    /// generation are excluded from that startup snapshot.
    RestartGuardReady {
        session_id: String,
        generation: u64,
        ack: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// A dangerous mode change is proven not to have taken effect; flip back the bit that was
    /// persisted before it was queued.
    ///
    /// Two kinds of evidence may send this note: the cancellation won the CAS (see
    /// `rc::ticket`), or the harness explicitly refused the target mode after the ticket was
    /// accepted. A timeout, an I/O error and an incomplete response may not, because none of
    /// them knows whether the mode change took effect. `arm` numbers that arming: the same
    /// session may have been armed again while this note is in flight, and a number that does
    /// not match says the bit to flip is not the current one.
    DangerDisarmed {
        session_id: String,
        generation: u64,
        arm: u64,
    },
    /// Notification-only acceptance consumed a pre-armed token. Persist the
    /// actual mode while that token continues forcing Plan on restart.
    ObserveTurnGuard {
        session_id: String,
        generation: u64,
        confirmation_token: String,
        ack: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// A late exact response agreed. Clear only the matching durable override;
    /// a stale confirmation must never clear a newer uncertainty.
    ConfirmTurnRestartGuard {
        session_id: String,
        generation: u64,
        confirmation_token: String,
        ack: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// A late response contradicted an already-ACKed inferred acceptance.
    /// Reassert the already-durable Plan barrier before publishing Ended.
    FailClosedTurnGuard {
        session_id: String,
        generation: u64,
        confirmation_token: String,
        ack: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    Bound {
        session_id: String,
        /// Which generation reported it. Same reason as `Ended` — a supervisor can queue
        /// `Bound` and then sit for a long time inside lineage landing (network + clone); if
        /// the same logical id is brought back up meanwhile, this stale note rewrites the
        /// **new** generation's live entry and roster.
        generation: u64,
        runtime_thread_id: String,
        /// The directory this session actually runs in. **Reported by the session itself**,
        /// not inferred by the daemon from the workspace: a recovered session's cwd need not
        /// equal the project root, and this roster cell is the value used to bring it back up
        /// after a restart.
        cwd: String,
        /// `owner/name@branch`, or None when no project is bound.
        agit_session: Option<String>,
        /// Immutable ID paired with `agit_session`; both or neither.
        expected_agent_id: Option<String>,
        /// The harness process writing the conversation, by pid and start time, when known: a
        /// lease waiting on this writer judges by it whether the writer is gone.
        writer: Option<(u32, i64)>,
    },
}

/// Proof carried with an approval command about its durable danger ledger.
///
/// `Persisted { arm: None }` is distinct from `NotRequired`: it means this
/// session was already durably dangerous, so a bypass suggestion may proceed
/// but this particular command owns nothing it may roll back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DangerAuthorization {
    NotRequired,
    Persisted { arm: Option<u64> },
}

impl DangerAuthorization {
    pub fn persisted(arm: Option<u64>) -> Self {
        Self::Persisted { arm }
    }

    pub fn is_persisted(self) -> bool {
        matches!(self, Self::Persisted { .. })
    }

    pub fn arm(self) -> Option<u64> {
        match self {
            Self::Persisted { arm } => arm,
            Self::NotRequired => None,
        }
    }
}

/// What the daemon asks a session to do.
pub enum Command {
    Runtime {
        name: String,
        arguments: serde_json::Value,
        reply: Ticket<serde_json::Value>,
    },
    Model {
        model: Option<crate::rc::harness::models::ModelPatch>,
        reply: Ticket<serde_json::Value>,
    },
    /// Internal startup barrier for a resumed Claude generation whose launch
    /// argv was forced to Plan by inherited ambiguous turn guards.
    ///
    /// Claude does not emit its native Ready frame until it receives the first
    /// user message. Waiting for that frame would deadlock a no-prompt resume:
    /// the daemon blocks Drive while the inherited guard exists, so no first
    /// message can arrive. The daemon queues this marker only after installing
    /// the generation in `Live`; consuming it proves both that the supervisor
    /// command loop is running and that the fixed Plan launch already happened.
    ClaudeRestartGuardReady,
    /// Creation-time prompt. Unlike a viewer Turn RPC, this has no receiver and
    /// may wait for the Codex Ready handshake in a dedicated single slot.
    InitialTurn {
        message: String,
        attribution: MessageAttribution,
    },
    Turn {
        message: String,
        attribution: MessageAttribution,
        guard_attempt: Option<crate::rc::harness::TurnGuardAttempt>,
        reply: Ticket<TurnStartOutcome>,
    },
    Enqueue {
        request: crate::rc::native_queue::Request,
        reply: Ticket<serde_json::Value>,
    },
    Steer {
        message: String,
        attribution: MessageAttribution,
        reply: Ticket<Delivery>,
    },
    Interrupt {
        expected_turn_id: Option<String>,
        reply: Ticket<()>,
    },
    Approve {
        response: ApprovalResponse,
        /// Whether the hub's stamp says owner. **Not read from `response`** — that whole
        /// structure comes from the browser.
        caller_is_owner: bool,
        /// Whether a mode-changing session approval has crossed the durable
        /// danger boundary, and which newly-created arm this command owns.
        danger: DangerAuthorization,
        reply: Ticket<ApprovalOutcome>,
    },
    SetPermissionMode {
        mode: PermissionMode,
        by: Option<String>,
        /// Whether this mode change is what **newly** set `ever_dangerous` true, and which
        /// arming it was.
        ///
        /// `Some(epoch)` means the bit was flipped from false to true just before this command
        /// was queued. If the command is cancelled (proving it never ran), the daemon flips the
        /// bit back — otherwise a session where nothing happened is marked dangerous forever.
        /// `None` means it was already dangerous: that bit does not belong to this mode change
        /// and nobody may flip it.
        ///
        /// It carries the epoch because the cancellation note is asynchronous: the same session
        /// may have been armed again while the note is in flight, and flipping back then erases
        /// the **new** arming. See `daemon::Live::danger_arm`.
        armed: Option<u64>,
        reply: Ticket<PermissionModeOutcome>,
    },
    /// The sender of a message waiting for the next turn is still allowed to send it.
    Keep { attribution: MessageAttribution },
    /// Stop at a person's request: a turn it cuts is held like their own interrupt.
    Shutdown,
    /// Stop that nobody chose for the running turn: it ends where agit stops it.
    ShutdownUnattended,
}

/// Accept the ticket at dequeue: `false` = the caller already cancelled, so **this command has
/// never run and must not run**.
///
/// `Shutdown` carries no ticket and always counts as accepted — it is a shutdown, and nobody
/// listening is no reason to skip it.
///
/// Testing `reply.is_closed()` is wrong in both directions: `SetPermissionMode` needs a special
/// case for "never discard" (so a timed-out bypass suddenly takes effect after the web has
/// already been told `SessionBusy`), and every other variant still has a gap between the test
/// and the start of execution. A CAS has no gap and needs no special case — see `rc::ticket`.
fn accept(c: &Command) -> bool {
    match c {
        Command::ClaudeRestartGuardReady => true,
        Command::InitialTurn { .. } | Command::Keep { .. } => true,
        Command::Model { reply, .. }
        | Command::Runtime { reply, .. }
        | Command::Enqueue { reply, .. } => reply.accept(),
        Command::Turn { reply, .. } => reply.accept(),
        Command::Steer { reply, .. } => reply.accept(),
        Command::Interrupt { reply, .. } => reply.accept(),
        Command::Approve { reply, .. } => reply.accept(),
        Command::SetPermissionMode { reply, .. } => reply.accept(),
        Command::Shutdown | Command::ShutdownUnattended => true,
    }
}

impl Command {
    /// The account whose admitted request this is, when it names one.
    fn sender_account(&self) -> Option<&str> {
        match self {
            Command::InitialTurn { attribution, .. }
            | Command::Turn { attribution, .. }
            | Command::Steer { attribution, .. }
            | Command::Keep { attribution } => attribution
                .sender
                .as_ref()
                .map(|sender| sender.account_id.as_str()),
            _ => None,
        }
    }

    /// The sender's own id for the words this command carries, when it carries words.
    pub(crate) fn client_msg_id(&self) -> Option<&str> {
        match self {
            Command::Turn { attribution, .. } | Command::Steer { attribution, .. } => {
                attribution.client_msg_id.as_deref()
            }
            _ => None,
        }
    }
}

fn command_danger_arm(command: &Command) -> Option<u64> {
    match command {
        Command::Approve { danger, .. } => danger.arm(),
        Command::SetPermissionMode { armed, .. } => *armed,
        _ => None,
    }
}

/// Read a batch of new lines **and push the watermark to the file's current position** — both
/// in one step.
///
/// # Why this is one function
///
/// Split apart, the order becomes "return early when there is no whole line", with the
/// watermark update behind that return. While the transcript is **writing the last record in
/// pieces** (the harness spends more than one syscall on a line), `poll()` advances the file
/// position but hands back no line, the watermark stays put, and the settlement's quiet test
/// sees the poll go quiet twice and declares the turn over — while the file is still growing,
/// and that record is cut outside the commit.
///
/// The position is a fact of the tailer, not a by-product of how many lines came back this
/// time. As one function, the call site has no choice about which to do first and so cannot
/// choose wrong.
fn poll_and_mark(tailer: &mut Tailer, watermark: &mut u64) -> Vec<crate::rc::tail::TailedLine> {
    let lines = tailer.poll().unwrap_or_default();
    *watermark = tailer.consumed();
    lines
}

/// An accepted interrupt makes every approval from the interrupted turn
/// unanswerable. Only that transition should reopen an approval-gated composer:
/// an interrupt with nothing to abandon must not invent a status change.
fn status_after_approval_interrupt(
    current: SessionStatus,
    abandoned: bool,
) -> Option<SessionStatus> {
    (abandoned && current == SessionStatus::AwaitingApproval).then_some(SessionStatus::Running)
}

/// Build the generation-fenced rollback notice only from a proven refusal.
/// Unknown outcomes deliberately keep the pre-persisted danger bit armed.
fn danger_disarm_after_mode_result(
    session_id: &str,
    generation: u64,
    armed: Option<u64>,
    result: &Result<PermissionApply, PermissionModeChangeError>,
) -> Option<SessionNote> {
    result
        .as_ref()
        .err()
        .is_some_and(PermissionModeChangeError::is_explicit_refusal)
        .then(|| {
            armed.map(|arm| SessionNote::DangerDisarmed {
                session_id: session_id.to_string(),
                generation,
                arm,
            })
        })
        .flatten()
}

impl Session {
    pub(crate) fn shared_executor(&self) -> bool {
        self.driver.shared_executor()
    }
    pub(crate) fn started_process(&self) -> bool {
        self.driver.started_process()
    }

    /// Drop both halves of every approval the harness can no longer answer.
    ///
    /// The supervisor copy is the machine-side authorization evidence; the
    /// driver copy is the native request id/suggestion needed to write the
    /// answer. They form one lifecycle even though their payloads differ, so a
    /// turn boundary must clear both in the same production function.
    async fn abandon_pending_approvals(&mut self) -> bool {
        let pending = std::mem::take(&mut self.pending);
        let driver = self.driver.abandon_pending_approvals();
        let abandoned = !pending.is_empty() || driver != 0;
        // Every viewer must retire the same cards when native requests become unanswerable.
        for approval_id in pending.into_keys() {
            self.emit(
                method::APPROVAL_RESOLVED,
                crate::protocol::ApprovalResolved {
                    session_id: self.info.session_id.clone(),
                    approval_id,
                },
            )
            .await;
        }
        abandoned
    }

    /// Start a session.
    ///
    /// Failure comes back as [`crate::rc::harness::proc::LaunchError`] rather than a bare
    /// `anyhow::Error`: the caller (`Daemon::spawn_session`) decides from whether the OS spawn
    /// happened at all whether the keyed `session.start` reservation is cancelled or leaves a
    /// permanent tombstone, and only the layer that actually calls `Command::spawn` knows that
    /// fact. Nothing here judges anything, it only forwards — **this function is not the
    /// materialization boundary**; what it calls is.
    ///
    /// The parameters are the full set of preconditions for this launch: identity, spec, the
    /// three outbound channels, the confinement scope, settlement state, generation, and the
    /// secret filter. Bundling them into a struct only moves where they sit — each one still
    /// has to be checked against its construction site, and the daemon is this function's only
    /// caller.
    #[allow(clippy::too_many_arguments)]
    pub async fn launch(
        mut info: SessionInfo,
        spec: LaunchSpec,
        out: mpsc::Sender<Frame>,
        notes: mpsc::Sender<SessionNote>,
        confinement: tokio::sync::watch::Receiver<crate::rc::Confinement>,
        settlement: tokio::sync::watch::Receiver<SettlementState>,
        generation: u64,
    ) -> Result<Session, crate::rc::harness::proc::LaunchError> {
        let agit_session = spec.agit_session.clone();
        let cwd = spec.cwd.clone();
        // The session is the owner's own, shown live to them or to colleagues in their
        // workspace: nothing it shows is checked (see `Redactor::for_device_control`).
        let redactor = redact::Redactor::this_machine().for_device_control();
        // **Whether this is a new run or a continuation is known at this moment.**
        //
        // The two sites below cannot ask "does the transcript file exist right now": that
        // moment falls after `bind_if_known()` (HTTP plus possibly a git clone) — by which time
        // the harness may long since have created the file and written the queue records and
        // the user's first prompt into it. Judged "already exists", the tailer seeks to the end
        // of the file and swallows that whole stretch: the session has no name on the page, and
        // its first sentence is exactly what was swallowed.
        //
        // The test must not be a time-sensitive observation, but the **intent** of this
        // launch.
        let resuming = spec.resume_from.is_some();
        let asked_mode = spec.permission_mode;
        let launching = std::time::Instant::now();
        let source_context = info
            .native_source
            .as_ref()
            .map(|source| {
                let registry = crate::rc::runtime_sources::Registry::open()?;
                registry.pin_for_launch(source)?;
                let context = crate::rc::runtime_context::RuntimeContext::resolve(
                    &registry,
                    &source.source_id,
                )?;
                anyhow::ensure!(
                    info.runtime == "codex" && context.source.generation == source.generation,
                    "native source changed before attachment"
                );
                Ok::<_, anyhow::Error>(context)
            })
            .transpose()
            .map_err(crate::rc::harness::proc::LaunchError::not_spawned)?;
        if spec.attach_only && source_context.is_some() {
            return Err(crate::rc::harness::proc::LaunchError::not_spawned(
                crate::rc::harness::AttachOnlyNotRunning.into(),
            ));
        }
        let mut driver = match &source_context {
            Some(context) => {
                let source = context
                    .source
                    .native()
                    .map_err(crate::rc::harness::proc::LaunchError::not_spawned)?;
                AnyDriver::Codex(Box::new(
                    crate::rc::harness::codex::CodexDriver::launch_shared(spec, &source).await?,
                ))
            }
            None => AnyDriver::launch(&info.runtime, spec).await?,
        };
        trace_phase(&info.session_id, "native.launch", launching);
        if let AnyDriver::Codex(codex) = &mut driver {
            let opening = std::time::Instant::now();
            codex.confirm_opening().await?;
            trace_phase(&info.session_id, "native.opening", opening);
        }
        if let Some(context) = &source_context {
            let validation = (|| -> crate::Result<()> {
                context.validate()?;
                anyhow::ensure!(
                    info.dangerous
                        || (driver.permission_mode_known()
                            && driver.permission_mode() != crate::protocol::PermissionMode::Bypass),
                    "native permissions changed during attachment; refresh the session before driving"
                );
                anyhow::ensure!(
                    opened_as_asked(
                        asked_mode,
                        driver
                            .permission_mode_known()
                            .then(|| driver.permission_mode())
                    ),
                    "native permissions changed during attachment; refresh the session before driving"
                );
                let roots = confinement.borrow().roots.clone();
                let native = driver
                    .runtime_thread_id()
                    .ok_or_else(|| anyhow::anyhow!("native attachment has no thread"))?;
                let thread = context.locate(&native, &roots)?;
                anyhow::ensure!(
                    thread.cwd == cwd,
                    "native conversation directory changed during attachment"
                );
                crate::rc::local_goal::validate_header(&thread.transcript, &native, &cwd)
            })();
            if let Err(error) = validation {
                let _ = driver.shutdown().await;
                return Err(crate::rc::harness::proc::LaunchError::spawned(error));
            }
        }
        info.interrupt_fenced = Some(driver.interrupt_fenced());
        record_opening_mode(&driver, &mut info);
        if driver.has_active_turn() {
            info.status = SessionStatus::Running;
        }
        let mut s = Session {
            agit_session,
            cwd,
            info,
            driver,
            stopped_in_turn: false,
            background_poll_at: tokio::time::Instant::now(),
            transcript_readable: false,
            tailer: None,
            rollover_check_at: tokio::time::Instant::now(),
            native_records: native_records::NativeRecords::default(),
            redactor,
            out,
            pending: Default::default(),
            consumed_bytes: 0,
            landed_thread: None,
            landing: None,
            archive_handoff: None,
            notes,
            confinement,
            settlement,
            pending_settlement: None,
            publication: None,
            publication_retry: PublicationRetry::default(),
            privacy_claim: None,
            reported_settlement_failure: None,
            local_settlement: None,
            settlement_due: false,
            stop_requested: false,
            stopping_turn: None,
            stop_turn_grace: STOP_TURN_GRACE,
            stop_unattended: false,
            completed_boundary: None,
            settlement_child: None,
            queued_initial_turn: None,
            pending_turn_command: None,
            pending_steer_command: None,
            steer_queue: Default::default(),
            absorbing_steers: std::collections::VecDeque::new(),
            turns_started: 0,
            started_prompts: std::collections::VecDeque::new(),
            pending_interrupt_command: None,
            resolved_initial_turn: None,
            announced_turn_ids: Default::default(),
            alerted_registered: Default::default(),
            generation,
            publication_incarnation: None,
            resuming,
        };
        // Announce only during launch. Native binding is repeated after the daemon
        // registers this generation, before repository landing and settlement.
        s.announce_binding_without_waiting().await;
        Ok(s)
    }

    /// Assemble `SessionNote::Bound`. With no runtime thread id yet there is nothing to
    /// announce.
    fn binding_note(&self) -> Option<SessionNote> {
        let thread_id = self.driver.runtime_thread_id()?;
        Some(SessionNote::Bound {
            session_id: self.info.session_id.clone(),
            generation: self.generation,
            runtime_thread_id: thread_id,
            cwd: self.cwd.to_string_lossy().into_owned(),
            agit_session: self.agit_session.as_ref().map(|l| l.to_string()),
            expected_agent_id: self.agit_session.as_ref().map(|l| l.agent_id().to_string()),
            writer: self.writer_process(),
        })
    }

    /// The harness process by pid and start time, while it runs.
    fn writer_process(&self) -> Option<(u32, i64)> {
        #[cfg(unix)]
        {
            self.driver
                .harness_pid()
                .and_then(|pid| Some((pid, crate::rc::native_claude::started_ms(pid)?)))
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// Tell the daemon the harness's own id. Cheap, idempotent, callable at any time.
    ///
    /// **Only callable once out of the daemon's global lock** — on a full channel it waits for
    /// room, see [`Session::announce_binding_without_waiting`].
    async fn announce_binding(&mut self) {
        let Some(note) = self.binding_note() else {
            return;
        };
        let _ = self.notes.send(note).await;
    }

    /// Launch does not wait for note delivery: the generation may not be registered yet.
    /// The supervisor repeats the binding before landing and every settlement.
    async fn announce_binding_without_waiting(&mut self) {
        let Some(note) = self.binding_note() else {
            return;
        };
        let _ = self.notes.try_send(note);
    }

    /// The executable for settlement and landing subprocesses. See [`SettlementChild`].
    fn settlement_exe(&self) -> Option<SettlementProgram> {
        let exe = match &self.settlement_child {
            Some(child) => Some(child.exe.clone()),
            // An installation can replace or unlink the daemon's pathname while it runs.
            None => crate::infra::own_image::own_image().ok(),
        }?;
        Some(SettlementProgram {
            exe,
            #[cfg(all(test, unix))]
            script: self.settlement_child.is_some(),
        })
    }

    /// This session's repo directory on this machine. See [`SettlementChild`].
    fn settlement_repo_dir(
        &self,
        agit_session: &crate::rc::lineage::AgitSession,
    ) -> Option<PathBuf> {
        match &self.settlement_child {
            Some(child) => Some(child.repo_dir.clone()),
            None => agit_session.repo_dir().ok(),
        }
    }

    /// Where the record of an automatic upload of `slug@branch` is kept. A settlement child a test
    /// supplied keeps it beside its repository, for the reason the child exists: the agit home is
    /// process-wide.
    fn upload_place(
        &self,
        slug: &str,
        branch: &str,
    ) -> crate::Result<crate::commands::upload_outcome::Place> {
        #[cfg(test)]
        if let Some(child) = &self.settlement_child {
            let records = child.repo_dir.join(".git").join("agit-test-uploads");
            return Ok(crate::commands::upload_outcome::Place::at(
                &records, &records,
            ));
        }
        crate::commands::upload_outcome::Place::of(slug, branch)
    }

    /// The harness-native session/thread id, once the driver knows it.
    /// claude-code knows it at launch (we chose it); codex learns it on Ready.
    pub fn runtime_thread_id(&self) -> Option<String> {
        self.driver.runtime_thread_id()
    }

    /// Broadcast the driver's current mode if it has drifted from what viewers
    /// were last told. Cheap enough to call after anything that might move it.
    async fn announce_mode(&mut self, by: Option<String>) {
        if !self.driver.permission_mode_known() {
            return;
        }
        let now = self.driver.permission_mode();
        if self.info.permission_mode == Some(now) {
            return;
        }
        self.info.permission_mode = Some(now);
        self.emit(
            method::SESSION_PERMISSION_MODE,
            SessionPermissionMode {
                native_default: false,
                session_id: self.info.session_id.clone(),
                mode: Some(now),
                applied: PermissionApply::Immediate,
                by,
            },
        )
        .await;
    }

    async fn prove_harness_tree_terminated(&mut self, message: &str) {
        let mut retry = std::time::Duration::from_millis(100);
        loop {
            match self.driver.shutdown().await {
                Ok(()) => break,
                Err(error) => {
                    tracing_note(&format!(
                        "[{}] {message}; harness cleanup is not yet proven: {error}",
                        self.info.session_id
                    ));
                    tokio::time::sleep(retry).await;
                    retry = retry
                        .saturating_mul(2)
                        .min(std::time::Duration::from_secs(2));
                }
            }
        }
        self.abandon_pending_approvals().await;
    }

    async fn terminate_after_unknown_native_write(&mut self, message: &str) {
        self.prove_harness_tree_terminated(message).await;
        self.set_status(SessionStatus::Ended).await;
    }

    /// Synchronize a restart-guard transition with the daemon without holding
    /// its mutex during disk retry. The supervisor keeps the native lifecycle
    /// paused until an exact generation-fenced save has acknowledged it.
    async fn sync_turn_guard(&self, barrier: TurnGuardBarrier) {
        let mut retry = std::time::Duration::from_millis(100);
        loop {
            let (ack, receipt) = tokio::sync::oneshot::channel();
            let note = match barrier.clone() {
                TurnGuardBarrier::NativeSettings { mode } => SessionNote::NativeSettings {
                    session_id: self.info.session_id.clone(),
                    generation: self.generation,
                    mode,
                    ack,
                },
                TurnGuardBarrier::Ready => SessionNote::RestartGuardReady {
                    session_id: self.info.session_id.clone(),
                    generation: self.generation,
                    ack,
                },
                TurnGuardBarrier::Observe { confirmation_token } => SessionNote::ObserveTurnGuard {
                    session_id: self.info.session_id.clone(),
                    generation: self.generation,
                    confirmation_token,
                    ack,
                },
                TurnGuardBarrier::Confirm { confirmation_token } => {
                    SessionNote::ConfirmTurnRestartGuard {
                        session_id: self.info.session_id.clone(),
                        generation: self.generation,
                        confirmation_token,
                        ack,
                    }
                }
                TurnGuardBarrier::FailClosed { confirmation_token } => {
                    SessionNote::FailClosedTurnGuard {
                        session_id: self.info.session_id.clone(),
                        generation: self.generation,
                        confirmation_token,
                        ack,
                    }
                }
            };
            let result = match self.notes.send(note).await {
                Ok(()) => receipt.await.unwrap_or_else(|_| {
                    Err("daemon dropped the restart-guard acknowledgement".into())
                }),
                Err(_) => Err("daemon dropped the restart-guard note channel".into()),
            };
            match result {
                Ok(()) => return,
                Err(error) => {
                    tracing_note(&format!(
                        "[{}] restart guard is not yet durable: {error}",
                        self.info.session_id
                    ));
                    tokio::time::sleep(retry).await;
                    retry = retry
                        .saturating_mul(2)
                        .min(std::time::Duration::from_secs(2));
                }
            }
        }
    }

    /// Resolve one native turn-start outcome only after every earlier native
    /// line has passed through `on_harness_event`.
    async fn resolve_turn_start(&mut self, pending: PendingTurnCommand, outcome: TurnStartOutcome) {
        self.resolve_turn_start_inner(pending, outcome, false).await;
    }

    async fn resolve_turn_start_inner(
        &mut self,
        pending: PendingTurnCommand,
        mut outcome: TurnStartOutcome,
        native_tree_already_terminated: bool,
    ) {
        if let TurnStartOutcome::Accepted { consumed_mode, .. } = &outcome {
            let expected_mode = pending
                .guard_attempt
                .as_ref()
                .map(|attempt| attempt.expected_mode);
            if *consumed_mode != expected_mode {
                outcome = TurnStartOutcome::Unknown {
                    message: format!(
                        "the harness reported consumed mode {consumed_mode:?}, but the durable turn guard expected {expected_mode:?}"
                    ),
                    attempted_mode: expected_mode.or(*consumed_mode),
                };
            }
        }
        // **Only `RetryableNotAccepted` may go back into this slot.**
        //
        // The slot's only drain is `flush_initial_turn_if_ready`, which runs on `Ready`, on
        // `TurnCompleted`, and when the browser resends the exact same sentence. Putting in a
        // sentence that can never be taken out again is not keeping it for later, it **wedges
        // the session**: a non-empty slot makes the `Command::Turn` path turn back every later
        // turn whose **text differs** (answering "the creation prompt is still waiting for
        // Codex"), while the creation prompt itself is never sent either.
        //
        // Every reason `RetryableNotAccepted` carries (the thread is still opening, a turn is
        // still running, a start has not settled yet) matches a boundary the supervisor can
        // see, and those boundaries take it out and retry it once. `ExplicitRefusal` is not
        // "not yet", it is the native runtime saying no to this sentence itself — resending it
        // unchanged only earns another refusal. It ends here (already logged above, with the
        // result handed back to the RPC receipt below), leaving the slot empty so this session
        // stays usable.
        if pending.initial
            && matches!(outcome, TurnStartOutcome::RetryableNotAccepted { .. })
            && self.queued_initial_turn.is_none()
        {
            self.queued_initial_turn = Some(InitialTurn {
                message: pending.message.clone(),
                attribution: pending.attribution.clone(),
            });
        }
        if pending.initial
            && pending.reply.is_some()
            && matches!(outcome, TurnStartOutcome::RetryableNotAccepted { .. })
        {
            outcome = TurnStartOutcome::ConcurrentNotAccepted {
                message: "this RPC was not accepted, but the creation prompt remains scheduled and must not be resubmitted".into(),
            };
        }
        let mut prepared_turn_started = None;
        if let TurnStartOutcome::Accepted {
            turn_id,
            still_running: true,
            consumed_mode,
            ..
        } = &outcome
        {
            match self.prepare_turn_started_once(
                turn_id.clone(),
                Some(pending.message.clone()),
                Some((pending.attribution.clone(), pending.message.clone())),
                true,
            ) {
                Ok(prepared) => prepared_turn_started = prepared,
                Err(message) => {
                    let attempted_mode = pending
                        .guard_attempt
                        .as_ref()
                        .map(|attempt| attempt.expected_mode)
                        .or(*consumed_mode);
                    outcome = TurnStartOutcome::Unknown {
                        message,
                        attempted_mode,
                    };
                }
            }
        }
        if let TurnStartOutcome::Accepted {
            consumed_mode: Some(effective_mode),
            confirmation: crate::rc::harness::TurnStartConfirmation::NotificationOnly,
            ..
        } = &outcome
        {
            let attempt = pending
                .guard_attempt
                .as_ref()
                .filter(|attempt| attempt.expected_mode == *effective_mode && !pending.initial);
            if let Some(attempt) = attempt {
                // The token was durable before dispatch. Record that native
                // consumed its expected mode while leaving the token armed
                // until the late exact response agrees.
                self.sync_turn_guard(TurnGuardBarrier::Observe {
                    confirmation_token: attempt.token.clone(),
                })
                .await;
            }
        }
        if self.driver.shared_executor()
            && !native_tree_already_terminated
            && pending.guard_attempt.is_none()
            && let TurnStartOutcome::Unknown {
                message,
                attempted_mode: None,
            } = &outcome
        {
            outcome = TurnStartOutcome::SharedUnknown {
                message: message.clone(),
            };
        }
        let ends_session = matches!(
            &outcome,
            TurnStartOutcome::Unknown { .. } | TurnStartOutcome::FatalNotAccepted { .. }
        );
        match &outcome {
            TurnStartOutcome::NativeSubmission {
                operation_id,
                delivery,
            } => {
                self.emit(
                    "session.messageAccepted",
                    serde_json::json!({"operation_id":operation_id,"client_msg_id":pending.attribution.client_msg_id,"delivery":delivery}),
                )
                .await;
            }
            TurnStartOutcome::Accepted { .. } => {
                self.announce_mode(pending.attribution.by.clone()).await;
                if let Some(prepared) = prepared_turn_started {
                    self.publish_turn_started(prepared).await;
                }
            }
            TurnStartOutcome::ExplicitRefusal { message, .. } => {
                tracing_note(&format!(
                    "[{}] turn start was refused: {message}",
                    self.info.session_id
                ));
            }
            TurnStartOutcome::RetryableNotAccepted { .. }
            | TurnStartOutcome::ConcurrentNotAccepted { .. }
            | TurnStartOutcome::SharedUnknown { .. } => {}
            TurnStartOutcome::FatalNotAccepted { message } => {
                // Request-id exhaustion is known to precede native I/O, but
                // this generation can never allocate another unique id. Retire
                // it before returning the typed fatal result; the daemon may
                // safely clear any pre-dispatch sticky guard attempt.
                if !native_tree_already_terminated {
                    self.prove_harness_tree_terminated(message).await;
                }
            }
            TurnStartOutcome::Unknown { message, .. } => {
                // Retrying could duplicate a prompt that Codex already began.
                // Prove the whole process tree gone before releasing the RPC
                // receipt; the daemon then makes any attempted mode fail-closed
                // and durable while it still owns the per-session gate.
                if !native_tree_already_terminated {
                    self.prove_harness_tree_terminated(message).await;
                }
            }
        }
        if pending.initial
            && pending.reply.is_none()
            && matches!(
                outcome,
                TurnStartOutcome::Accepted {
                    still_running: true,
                    ..
                }
            )
        {
            self.resolved_initial_turn = Some(ResolvedInitialTurn {
                message: pending.message.clone(),
                attribution: pending.attribution.clone(),
                outcome: outcome.clone(),
            });
        }
        if let Some(reply) = pending.reply {
            reply.finish(Ok(outcome));
        }
        if ends_session {
            self.set_status(SessionStatus::Ended).await;
        }
    }

    async fn resolve_steer(
        &mut self,
        pending: PendingSteerCommand,
        result: crate::Result<Delivery>,
    ) {
        if let Some(message) = result
            .as_ref()
            .err()
            .filter(|error| crate::rc::harness::is_request_id_exhaustion(error))
            .map(ToString::to_string)
        {
            self.handle_protocol_invariant(message, None, None).await;
        } else if let Ok(delivery) = result.as_ref() {
            if !self.driver.correlates_pending_prompt() {
                self.emit(
                    "session.messageAccepted",
                    serde_json::json!({
                        "client_msg_id":pending.attribution.client_msg_id,"delivery":delivery,
                    }),
                )
                .await;
            } else {
                self.emit(
                    method::TURN_STEERED,
                    crate::protocol::TurnSteered {
                        message: pending.message,
                        delivery: *delivery,
                        by: pending.attribution.by,
                        sender: pending.attribution.sender,
                        client_msg_id: pending.attribution.client_msg_id,
                        native_prompt_id: pending.attribution.native_prompt_id,
                    },
                )
                .await;
            }
        }
        pending.reply.finish(result);
    }

    async fn begin_turn_start(&mut self, mut pending: PendingTurnCommand) {
        let started = std::time::Instant::now();
        if self.pending_turn_command.is_some() {
            self.resolve_turn_start(
                pending,
                TurnStartOutcome::ConcurrentNotAccepted {
                    message: "another turn is still awaiting native acceptance".into(),
                },
            )
            .await;
            return;
        }
        pending.attribution.native_prompt_id = self.driver.reserve_prompt_identity();
        if let Some(prompt) = &pending.attribution.native_prompt_id {
            self.started_prompt(prompt.clone());
        }
        let dispatch = self
            .driver
            .start_turn(
                &pending.message,
                !pending.initial,
                pending.guard_attempt.clone(),
            )
            .await;
        trace_phase(&self.info.session_id, "native.turn_dispatch", started);
        match dispatch {
            TurnStartDispatch::Resolved(outcome) => {
                self.resolve_turn_start(pending, outcome).await;
            }
            TurnStartDispatch::Awaiting => {
                self.pending_turn_command = Some(pending);
            }
        }
    }

    fn coalesce_pending_initial_reply(
        &mut self,
        message: &str,
        attribution: &MessageAttribution,
        guard_attempt: Option<&TurnGuardAttempt>,
        reply: Ticket<TurnStartOutcome>,
    ) -> PendingInitialReply {
        // Another member's submission cannot consume the creator's result,
        // even when both people type the same text.
        if self
            .resolved_initial_turn
            .as_ref()
            .is_some_and(|resolved| !resolved.attribution.same_sender(attribution))
        {
            return PendingInitialReply::Absent(reply);
        }
        // A different viewer prompt establishes that this is no longer a
        // retry of creation; taking the old result here prevents it from
        // leaking into a later, intentional same-text turn.
        if self
            .resolved_initial_turn
            .as_ref()
            .is_some_and(|resolved| resolved.message == message)
        {
            if guard_attempt.is_some() {
                reply.finish(Ok(TurnStartOutcome::ConcurrentNotAccepted {
                    message: "the creation prompt is already running; the queued mode belongs to the following turn".into(),
                }));
            } else {
                let resolved = self
                    .resolved_initial_turn
                    .take()
                    .expect("matching resolved initial turn was checked above");
                reply.finish(Ok(resolved.outcome));
            }
            return PendingInitialReply::Attached;
        }
        self.resolved_initial_turn = None;
        let Some(pending) = self
            .pending_turn_command
            .as_mut()
            .filter(|pending| pending.initial)
        else {
            return PendingInitialReply::Absent(reply);
        };
        if pending.message != message
            || !pending.attribution.same_sender(attribution)
            || pending.reply.is_some()
        {
            return PendingInitialReply::Blocked(reply);
        }
        if guard_attempt.is_some() {
            reply.finish(Ok(TurnStartOutcome::ConcurrentNotAccepted {
                message: "the creation prompt is still awaiting native acceptance; the queued mode belongs to the following turn".into(),
            }));
            return PendingInitialReply::Attached;
        }
        pending.reply = Some(reply);
        PendingInitialReply::Attached
    }

    async fn flush_initial_turn_if_ready(&mut self) {
        if self.driver.runtime_thread_id().is_none() || self.pending_turn_command.is_some() {
            return;
        }
        if let Some(initial) = self.queued_initial_turn.take() {
            self.begin_turn_start(PendingTurnCommand {
                message: initial.message,
                attribution: initial.attribution,
                reply: None,
                initial: true,
                guard_attempt: None,
            })
            .await;
        }
    }

    async fn emit(&self, method: &str, params: impl serde::Serialize) {
        let f = Frame::notification(method, params);
        let _ = self.out.send(f).await;
    }

    /// One device-local rule alerts once per session. `secret.detected` carries only a count
    /// and a source; the rule id and the matched text stay on this machine.
    async fn alert_registered(&mut self, ids: Vec<String>, source: &str) {
        let mut fresh = 0usize;
        for id in ids {
            if self.alerted_registered.insert(id) {
                fresh += 1;
            }
        }
        if fresh > 0 {
            self.emit(
                method::SECRET_DETECTED,
                SecretDetected {
                    count: fresh,
                    source: source.to_string(),
                },
            )
            .await;
        }
    }

    #[cfg(test)]
    fn turn_was_announced(&self, turn_id: &str) -> bool {
        self.announced_turn_ids.contains(turn_id)
    }

    fn remember_announced_turn(&mut self, turn_id: String) -> Result<bool, String> {
        self.announced_turn_ids
            .try_insert(turn_id)
            .map_err(|error| format!("supervisor turn.started tombstone: {error}"))
    }

    /// Reserve one authoritative turn head before any mode/status/wire side
    /// effect. Capacity failure is a protocol fail-stop, not permission to
    /// evict an older identity and let a delayed duplicate resurrect it.
    fn prepare_turn_started_once(
        &mut self,
        turn_id: String,
        native_prompt: Option<String>,
        resolved_attribution: Option<(MessageAttribution, String)>,
        mark_running: bool,
    ) -> Result<Option<PreparedTurnStarted>, String> {
        crate::rc::harness::validate_native_turn_id(&turn_id)
            .map_err(|error| format!("supervisor turn.started: {error}"))?;
        if !self.remember_announced_turn(turn_id.clone())? {
            return Ok(None);
        }
        let (attribution, command_prompt) =
            if let Some((attribution, prompt)) = resolved_attribution {
                (attribution, Some(prompt))
            } else if let Some(pending) = self
                .pending_turn_command
                .as_ref()
                .filter(|_| self.driver.correlates_pending_prompt())
            {
                (pending.attribution.clone(), Some(pending.message.clone()))
            } else {
                (MessageAttribution::default(), None)
            };
        let prompt = native_prompt.or(command_prompt);
        let mut registered_ids = vec![];
        let prompt = prompt.map(|p| {
            let report = self.redactor.scrub(&p);
            registered_ids = report.registered_ids;
            report.text
        });
        Ok(Some(PreparedTurnStarted {
            frame: TurnStarted {
                turn_id,
                source: if attribution.by.is_some() || attribution.sender.is_some() {
                    TurnSource::Remote
                } else {
                    TurnSource::Local
                },
                by: attribution.by,
                sender: attribution.sender,
                client_msg_id: attribution.client_msg_id,
                native_prompt_id: attribution.native_prompt_id,
                prompt,
            },
            mark_running,
            registered_ids,
        }))
    }

    async fn publish_turn_started(&mut self, prepared: PreparedTurnStarted) {
        self.alert_registered(prepared.registered_ids, "turn_prompt")
            .await;
        if prepared.mark_running && self.pending.is_empty() {
            self.set_status(SessionStatus::Running).await;
        }
        self.emit(method::TURN_STARTED, prepared.frame).await;
    }

    /// Emit one authoritative turn head with the best attribution still
    /// available. Approval/completion evidence may call this before Codex's
    /// own late `turn/started`; the harness-lifetime id set makes that line a
    /// no-op rather than a second head or a Running resurrection.
    async fn announce_turn_started_once(
        &mut self,
        turn_id: String,
        native_prompt: Option<String>,
        resolved_attribution: Option<(MessageAttribution, String)>,
        mark_running: bool,
    ) -> Result<bool, String> {
        let Some(prepared) = self.prepare_turn_started_once(
            turn_id,
            native_prompt,
            resolved_attribution,
            mark_running,
        )?
        else {
            return Ok(false);
        };
        self.publish_turn_started(prepared).await;
        Ok(true)
    }

    async fn set_status(&mut self, status: SessionStatus) {
        if self.info.status == status {
            return;
        }
        if status == SessionStatus::Ended {
            self.stopped_in_turn = matches!(
                self.info.status,
                SessionStatus::Running | SessionStatus::AwaitingApproval
            );
            self.abandon_steers().await;
            // No more of this process's records will be read: what still waits for its copy
            // can't be named now.
            self.absorbing_steers.clear();
        }
        self.info.status = status;
        self.info.updated_at = chrono::Utc::now().to_rfc3339();
        self.emit(
            method::SESSION_STATUS,
            serde_json::json!({"session_id": self.info.session_id, "status": status}),
        )
        .await;
    }

    /// The writer was inside a turn when it stopped: either the supervisor ended it mid-turn, or
    /// the harness exited on its own while the session was still running a turn (that exit
    /// reports `ended` without passing through `set_status`).
    fn writer_in_turn(&self) -> bool {
        self.stopped_in_turn
            || matches!(
                self.info.status,
                SessionStatus::Running | SessionStatus::AwaitingApproval
            )
    }

    /// A reaped private harness can no longer write; record the transcript it left behind.
    /// A shared executor keeps writing after this client leaves, so it leaves no mark.
    fn record_writer_release(&self, id: &str) {
        if self.driver.shared_executor() {
            return;
        }
        let (Some(thread), Some(transcript)) = (
            self.driver.runtime_thread_id(),
            self.driver.transcript_path(),
        ) else {
            return;
        };
        if let Err(error) =
            crate::rc::writer_release::record(&self.info.runtime, &thread, &transcript)
        {
            tracing_note(&format!(
                "[{id}] could not record the released transcript: {error:#}"
            ));
        }
    }

    /// Run until the harness exits or we are told to stop.
    pub async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        let id = self.info.session_id.clone();
        let generation = self.generation;
        let notes = self.notes.clone();
        self.run_inner(&mut commands).await;
        let exiting = std::time::Instant::now();
        let log = |event: &str, extra: serde_json::Value| {
            let mut metadata = serde_json::json!({
                "session_id": crate::rc::diagnostics::id(&id),
                "generation": generation,
                "elapsed_ms": crate::rc::diagnostics::elapsed_ms(exiting),
            });
            if let (Some(metadata), serde_json::Value::Object(extra)) =
                (metadata.as_object_mut(), extra)
            {
                metadata.extend(extra);
            }
            crate::rc::diagnostics::event(event, metadata);
        };
        log(
            "supervisor.exiting",
            serde_json::json!({"in_turn": self.writer_in_turn()}),
        );
        // Final safety net for EOF/command-channel shutdown paths. The maps are
        // about native requests owned by this process; none survive its exit.
        self.abandon_pending_approvals().await;
        let thread = self.driver.runtime_thread_id();
        let process = self.writer_process();
        // **The reason for exiting does not decide whether the process tree is reaped.**
        //
        // An exit path that returns directly — the harness's own EOF/Exited, a closed command
        // channel — leaves the compilers, servers and background shells it started on the
        // machine. Funnelling into the single finally point means a newly added exit branch
        // cannot miss it either. A failed cleanup leaves a diagnostic behind but must not
        // swallow Ended — the daemon still has to remove a finished session from its table.
        let shutdown = self.driver.shutdown().await;
        log(
            "supervisor.harness_shutdown",
            serde_json::json!({"ok": shutdown.is_ok()}),
        );
        match shutdown {
            Ok(()) => {
                self.record_writer_release(&id);
                let _ = notes
                    .send(SessionNote::WriterReleased {
                        session_id: id.clone(),
                        generation,
                        thread,
                        in_turn: self.writer_in_turn(),
                    })
                    .await;
            }
            Err(e) => {
                tracing_note(&format!("[{id}] harness cleanup failed: {e}"));
                let _ = notes
                    .send(SessionNote::WriterNotReleased {
                        session_id: id.clone(),
                        generation,
                        thread,
                        process,
                    })
                    .await;
            }
        }
        // **A session ending is a settlement boundary too.** Funnelled into the same single
        // finally point: Shutdown, driver EOF and harness Exited each return on their own, and
        // adding the call to them one by one always misses one.
        log("supervisor.settle_started", serde_json::json!({}));
        self.settle_on_exit().await;
        log("supervisor.settle_finished", serde_json::json!({}));
        // **Every exit path announces.** Here rather than one line before each `return`,
        // because forgetting to announce on a newly added exit path has no symptom at all —
        // that row stays silently in `sessions` while its channel is dead.
        log("supervisor.ended", serde_json::json!({}));
        let _ = notes
            .send(SessionNote::Ended {
                session_id: id,
                generation,
            })
            .await;
    }

    fn turn_running(&self) -> bool {
        matches!(
            self.info.status,
            SessionStatus::Running | SessionStatus::AwaitingApproval
        )
    }

    /// Interrupt the running turn because the daemon is stopping, so the runtime closes it the
    /// way it closes any interrupt. False when no turn runs or the interrupt could not be sent;
    /// the stop then goes ahead at once. A runtime the daemon did not start, or shares, outlives
    /// the stop, and its turn is left to run.
    async fn interrupt_for_stop(&mut self) -> bool {
        if self.stopping_turn.is_some()
            || !self.turn_running()
            || !self.driver.started_process()
            || self.driver.shared_executor()
        {
            return false;
        }
        // A person's interrupt already on its way closes the same turn.
        if self.pending_interrupt_command.is_none() {
            match self.driver.interrupt(None).await {
                InterruptDispatch::Resolved(Ok(
                    crate::rc::harness::InterruptOutcome::Requested,
                ))
                | InterruptDispatch::Awaiting => {}
                InterruptDispatch::Resolved(_) => return false,
            }
        }
        self.stopping_turn = Some(tokio::time::Instant::now() + self.stop_turn_grace);
        true
    }

    async fn stop(&mut self) {
        self.stopping_turn = None;
        self.stop_requested = true;
        self.abandon_pending_approvals().await;
        self.set_status(SessionStatus::Ended).await;
    }

    async fn run_inner(&mut self, commands: &mut mpsc::Receiver<Command>) {
        // Native ownership is announced before commands run; repository preparation is independent.
        self.bind_if_known().await;
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(TAIL_POLL_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut deferred_command = None;
        // Whether `deferred_command` is a waiting message the session took up itself, not a
        // request the daemon sent.
        let mut dequeued = false;
        loop {
            if self
                .landing
                .as_ref()
                .is_some_and(|task| task.0.is_finished())
            {
                self.finish_landing().await;
            }
            // A message that waited for this turn's end goes as the next command, through every
            // check a command from the daemon passes.
            if deferred_command.is_none()
                && let Some(command) = self.next_queued_turn().await
            {
                deferred_command = Some(Some(command));
                dequeued = true;
            }
            tokio::select! {
                biased;
                cmd = async {
                    match deferred_command.take() {
                        Some(command) => command,
                        None => commands.recv().await,
                    }
                }, if self.stopping_turn.is_none() => {
                    let from_queue = std::mem::take(&mut dequeued);
                    // **Do not do it when nobody is waiting.**
                    //
                    // The daemon's wait for an answer is bounded (a slow settlement can hold it
                    // for tens of seconds), and on timeout it has already told the web that
                    // this session is busy and can be retried. The command is still sitting in
                    // this queue — it runs anyway once settlement ends, so the same sentence
                    // goes out twice and the same approval is answered twice, while the user
                    // believes the first one never happened.
                    //
                    // A `timeout` drops the `Receiver` with it, so one question here is
                    // enough.
                    let cmd = match cmd {
                        Some(c) if !accept(&c) => {
                            // The side that won the cancellation holds the fact that this
                            // never ran, and the danger bit was persisted **before** the
                            // command was queued: someone has to flip it back, or a session
                            // where nothing happened is marked dangerous forever.
                            if let Some(arm) = command_danger_arm(&c) {
                                let _ = self
                                    .notes
                                    .send(SessionNote::DangerDisarmed {
                                        session_id: self.info.session_id.clone(),
                                        generation: self.generation,
                                        arm,
                                    })
                                    .await;
                            }
                            continue;
                        }
                        other => other,
                    };
                    // An admitted request from an account says that account is still there, for
                    // its messages waiting for the next turn.
                    if let Some(command) = &cmd {
                        self.steer_queue.admitted(command, from_queue);
                    }
                    if let Some(command) = &cmd
                        && let Err(error) = self.prepare_fallback_instruction(command).await {
                            self.reject_fallback_instruction(cmd.expect("command was checked"), error).await;
                            continue;
                        }
                    match cmd {
                        None | Some(Command::Shutdown | Command::ShutdownUnattended) => {
                            // Recorded before the wait: a runtime that exits during it without
                            // ending the turn must still leave a turn this stop cut, not one that
                            // ended on its own. A person's interrupt already on its way wins.
                            self.stop_requested = true;
                            self.stop_unattended = matches!(cmd, Some(Command::ShutdownUnattended))
                                && self.pending_interrupt_command.is_none();
                            if self.interrupt_for_stop().await {
                                continue;
                            }
                            self.stop().await;
                            return;
                        }
                        Some(Command::ClaudeRestartGuardReady) => {
                            // This command is daemon-internal and is queued
                            // only after this exact generation is present in
                            // `Live`. Keep the loop parked until the inherited
                            // guard snapshot has been replaced by the launch
                            // argv's authoritative Plan mode on durable media.
                            debug_assert_eq!(self.info.runtime, "claude-code");
                            self.sync_turn_guard(TurnGuardBarrier::Ready).await;
                        }
                        Some(Command::InitialTurn { message, attribution }) => {
                            if self.driver.runtime_thread_id().is_none() {
                                // This is the one fire-and-forget path allowed
                                // to wait for Codex Ready. A viewer TURN_START
                                // always receives a retryable not-accepted
                                // result instead of a false success receipt.
                                if self.queued_initial_turn.is_none() {
                                    self.queued_initial_turn = Some(InitialTurn { message, attribution });
                                } else {
                                    tracing_note(&format!(
                                        "[{}] ignored a duplicate creation prompt while Codex was opening",
                                        self.info.session_id
                                    ));
                                }
                            } else {
                                self.begin_turn_start(PendingTurnCommand {
                                    message,
                                    attribution,
                                    reply: None,
                                    initial: true,
                                    guard_attempt: None,
                                })
                                .await;
                                if self.info.status == SessionStatus::Ended {
                                    return;
                                }
                            }
                        }
                        Some(Command::Turn {
                            message,
                            attribution,
                            guard_attempt,
                            reply,
                        }) => {
                            let reply = match self.coalesce_pending_initial_reply(
                                &message,
                                &attribution,
                                guard_attempt.as_ref(),
                                reply,
                            ) {
                                PendingInitialReply::Attached => continue,
                                PendingInitialReply::Blocked(reply) => {
                                    reply.finish(Ok(
                                        TurnStartOutcome::ConcurrentNotAccepted {
                                            message: "the creation prompt is already awaiting native acceptance; wait for its outcome before sending another turn".into(),
                                        },
                                    ));
                                    continue;
                                }
                                PendingInitialReply::Absent(reply) => reply,
                            };
                            if let Some(initial) = self.queued_initial_turn.take() {
                                if initial.message != message
                                    || !initial.attribution.same_sender(&attribution)
                                {
                                    self.queued_initial_turn = Some(initial);
                                    reply.finish(Ok(TurnStartOutcome::ConcurrentNotAccepted {
                                        message: "the creation prompt is still waiting for Codex; wait for it to start before sending another turn".into(),
                                    }));
                                    continue;
                                }
                                // This is a retry of the exact creation prompt,
                                // not a new turn. Preserve its original mode
                                // boundary: a mode queued later belongs to the
                                // following turn.
                                self.begin_turn_start(PendingTurnCommand {
                                    message,
                                    attribution: initial.attribution,
                                    reply: Some(reply),
                                    initial: true,
                                    guard_attempt,
                                })
                                .await;
                            } else {
                                self.begin_turn_start(PendingTurnCommand {
                                    message,
                                    attribution,
                                    reply: Some(reply),
                                    initial: false,
                                    guard_attempt,
                                })
                                .await;
                            }
                            if self.info.status == SessionStatus::Ended {
                                return;
                            }
                        }
                        Some(Command::Steer { message, mut attribution, reply }) => {
                            let report = self.redactor.scrub(&message);
                            self.alert_registered(report.registered_ids, "turn_steer").await;
                            if self.pending_steer_command.is_some() {
                                reply.finish(Err(anyhow::anyhow!("a native submission is awaiting its receipt")));
                                continue;
                            }
                            attribution.native_prompt_id = self.driver.reserve_prompt_identity();
                            let pending = PendingSteerCommand { message: report.text, attribution, reply };
                            if self.driver.steers_at_step() {
                                self.steer_at_step(message, pending).await;
                            } else if let Err(error) = self.admits_absorbing_steer() {
                                pending.reply.finish(Err(error));
                            } else {
                                match self.driver.steer(&message).await {
                                    SteerDispatch::Resolved(result) => {
                                        if matches!(result, Ok(Delivery::AtToolBoundary))
                                            && let Some(native_prompt_id) = pending.attribution.native_prompt_id.clone()
                                        {
                                            self.absorbing(native_prompt_id, message);
                                        }
                                        self.resolve_steer(pending, result).await
                                    }
                                    SteerDispatch::Awaiting => self.pending_steer_command = Some(pending),
                                }
                            }
                            if self.info.status == SessionStatus::Ended { return; }
                        }
                        Some(Command::Keep { .. }) => {}
                        Some(Command::Interrupt { expected_turn_id, reply }) => {
                            if self.pending_interrupt_command.is_some() {
                                reply.finish(Err(anyhow::anyhow!("a native stop is awaiting its receipt")));
                                continue;
                            }
                            match self.driver.interrupt(expected_turn_id.as_deref()).await {
                                InterruptDispatch::Resolved(result) => self.resolve_interrupt(reply, result).await,
                                InterruptDispatch::Awaiting => self.pending_interrupt_command = Some(reply),
                            }
                            if self.info.status == SessionStatus::Ended { return; }
                        }
                        Some(Command::Approve {
                            mut response,
                            caller_is_owner,
                            danger,
                            reply,
                        }) => {
                            // An unrecognized approval_id is always refused: forwarding an
                            // approval we never sent lets whoever guesses an id answer in
                            // someone else's place.
                            let Some(pending) = self.pending.remove(&response.approval_id)
                            else {
                                reply.finish(Ok(ApprovalOutcome::ExplicitRefusal {
                                    message: format!(
                                        "approval {} is not pending on this session",
                                        response.approval_id
                                    ),
                                    retained: false,
                                }));
                                continue;
                            };
                            // A colleague's "allow for this session" would change the session's
                            // permission mode, a setting that stays the owner's: it applies once.
                            if !caller_is_owner {
                                response.scope = crate::protocol::ApprovalScope::Once;
                            }
                            let changes_session_mode = matches!(
                                (response.decision, response.scope),
                                (
                                    crate::protocol::ApprovalDecision::Allow,
                                    crate::protocol::ApprovalScope::Session
                                )
                            );
                            if changes_session_mode
                                && pending.suggested_permission_mode.is_none()
                            {
                                // The browser cannot manufacture a session
                                // effect the machine never advertised. More
                                // importantly, an unmodelled native suggestion
                                // cannot cross the ledger boundary safely.
                                self.pending.insert(response.approval_id.clone(), pending);
                                reply.finish(Ok(ApprovalOutcome::ExplicitRefusal {
                                    message: "this approval has no fully understood session permission-mode suggestion".into(),
                                    retained: true,
                                }));
                                continue;
                            }
                            if changes_session_mode
                                && pending
                                    .suggested_permission_mode
                                    .is_some_and(PermissionMode::is_dangerous)
                                && !danger.is_persisted()
                            {
                                self.pending.insert(response.approval_id.clone(), pending);
                                reply.finish(Ok(ApprovalOutcome::ExplicitRefusal {
                                    message: "the dangerous session approval was not durably authorized".into(),
                                    retained: true,
                                }));
                                continue;
                            }
                            let expected_mode = changes_session_mode
                                .then_some(pending.suggested_permission_mode)
                                .flatten();
                            let mut outcome = self.driver.answer_approval(&response).await;
                            match &mut outcome {
                                ApprovalOutcome::Applied { effective_mode }
                                    if changes_session_mode
                                        && *effective_mode != expected_mode =>
                                {
                                    outcome = ApprovalOutcome::Unknown {
                                        message: "the harness reported an approval mode different from its machine-originated suggestion".into(),
                                        attempted_mode: expected_mode,
                                    };
                                }
                                ApprovalOutcome::Unknown { attempted_mode, .. }
                                    if changes_session_mode && attempted_mode.is_none() =>
                                {
                                    *attempted_mode = expected_mode;
                                }
                                _ => {}
                            }
                            match &outcome {
                                ApprovalOutcome::Applied { .. } | ApprovalOutcome::Resolved => {
                                    self.emit(method::APPROVAL_RESOLVED, crate::protocol::ApprovalResolved {
                                        session_id: self.info.session_id.clone(),
                                        approval_id: response.approval_id.clone(),
                                    }).await;
                                    if self.pending.is_empty() {
                                        self.set_status(SessionStatus::Running).await;
                                    }
                                    // Answering "allow, and stop asking" moves
                                    // the mode underneath us; tell every viewer.
                                    self.announce_mode(None).await;
                                }
                                ApprovalOutcome::ExplicitRefusal { .. } => {}
                                ApprovalOutcome::MalformedDecision { .. }
                                | ApprovalOutcome::Handed { .. }
                                | ApprovalOutcome::AwaitingResolution { .. } => {
                                    self.pending.insert(response.approval_id.clone(), pending);
                                }
                                ApprovalOutcome::Unknown { message, .. } => {
                                    // The same native write may already have
                                    // installed a sticky policy. Do not expose
                                    // the outcome until the process tree is gone;
                                    // the daemon then durably projects Plan.
                                    self.terminate_after_unknown_native_write(message).await;
                                }
                            }
                            let ended = self.info.status == SessionStatus::Ended;
                            reply.finish(Ok(outcome));
                            if ended {
                                return;
                            }
                        }
                        Some(Command::Runtime { name, arguments, reply }) => {
                            let result = self.driver.runtime_command(&name, arguments).await.map(|value|self.redactor.scrub_json(&value).value);
                            reply.finish(result);
                        }
                        Some(Command::Enqueue { request, reply }) => {
                            // A colleague's message queues as the owner's does, whatever the
                            // session ran in before.
                            let result = self.driver.enqueue(request).await;
                            reply.finish(result);
                        }
                        Some(Command::Model { model, reply }) => {
                            let result = self.driver.model_control(model.as_ref()).await;
                            if model.is_some() {
                                self.emit(method::SESSION_MODEL, serde_json::json!({
                                    "session_id": self.info.session_id,
                                })).await;
                            }
                            reply.finish(result);
                        }
                        Some(Command::SetPermissionMode {
                            mode,
                            by,
                            armed,
                            reply,
                        }) => {
                            let shared = self.driver.shared_executor();
                            let r = self.driver.set_permission_mode(mode).await;
                            if let Some(note) = danger_disarm_after_mode_result(
                                &self.info.session_id,
                                self.generation,
                                armed,
                                &r,
                            ) {
                                let _ = self.notes.send(note).await;
                            }
                            let outcome = match r {
                                Ok(applied) => {
                                    if shared {
                                        self.sync_turn_guard(TurnGuardBarrier::NativeSettings { mode: Some(mode) }).await;
                                    }
                                    self.info.permission_mode =
                                        Some(self.driver.permission_mode());
                                    self.emit(
                                        method::SESSION_PERMISSION_MODE,
                                        SessionPermissionMode {
                                            native_default: shared,
                                            session_id: self.info.session_id.clone(),
                                            mode: Some(mode),
                                            applied,
                                            by,
                                        },
                                    )
                                    .await;
                                    if shared { PermissionModeOutcome::SharedApplied { applied } }
                                    else { PermissionModeOutcome::Applied { applied } }
                                }
                                Err(error) if error.is_explicit_refusal() => {
                                    PermissionModeOutcome::ExplicitRefusal {
                                        message: error.to_string(),
                                    }
                                }
                                Err(error) if shared => {
                                    self.sync_turn_guard(TurnGuardBarrier::NativeSettings { mode: None }).await;
                                    self.info.permission_mode = None;
                                    self.info.dangerous = true;
                                    self.emit(method::SESSION_PERMISSION_MODE, SessionPermissionMode {
                                        session_id: self.info.session_id.clone(), mode: None,
                                        native_default: true, applied: crate::protocol::PermissionApply::Immediate, by,
                                    }).await;
                                    PermissionModeOutcome::SharedUnknown { message: error.to_string() }
                                }
                                Err(error) => {
                                    let message = format!(
                                        "permission-mode outcome is unknown: {error}"
                                    );
                                    self.terminate_after_unknown_native_write(&message).await;
                                    PermissionModeOutcome::Unknown { message }
                                }
                            };
                            let ended = self.info.status == SessionStatus::Ended;
                            reply.finish(Ok(outcome));
                            if ended {
                                return;
                            }
                        }
                    }
                }

                ev = self.driver.next_event() => {
                    match ev {
                        None => {
                            self.abandon_pending_approvals().await;
                            self.set_status(SessionStatus::Ended).await;
                            return;
                        }
                        Some(HarnessEvent::Exited { code }) => {
                            crate::rc::diagnostics::event(
                                "runtime.exited",
                                serde_json::json!({
                                    "session_id": crate::rc::diagnostics::id(&self.info.session_id),
                                    "generation": self.generation,
                                    "exit_code": code,
                                }),
                            );
                            self.abandon_pending_approvals().await;
                            // Drain whatever the harness wrote on its way out.
                            self.drain_transcript().await;
                            self.emit(method::SESSION_STATUS, serde_json::json!({
                                "session_id": self.info.session_id,
                                "status": SessionStatus::Ended,
                                "exit_code": code,
                            })).await;
                            return;
                        }
                        Some(e) => {
                            deferred_command = self.on_harness_event_with_commands(e, Some(commands)).await;
                            dequeued = false;
                            if self.info.status == SessionStatus::Ended {
                                return;
                            }
                            if self.stopping_turn.is_some() && !self.turn_running() {
                                self.stop().await;
                                return;
                            }
                        }
                    }
                }

                Some((attribution, outcome)) = self.steer_queue.outcomes.1.recv() => {
                    self.queued_turn_outcome(&attribution, outcome).await;
                }

                _ = ticker.tick() => {
                    if self
                        .stopping_turn
                        .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
                    {
                        self.stop().await;
                        return;
                    }
                    self.finish_local_settlement(false).await;
                    self.finish_publication(false).await;
                    if self.idle_settlement_ready() {
                        deferred_command = self.settle_until_command(commands).await;
                        dequeued = false;
                    }
                    // **The mode is a value to poll, not a series of events each call site
                    // must remember to broadcast.**
                    //
                    // codex can only deliver policy on `turn/start`, so a `next_turn` mode
                    // change becomes fact at some moment inside the driver. Hanging the
                    // broadcast on `Command::Turn` misses it: the first prompt **queues** while
                    // the thread id is not ready yet and is sent by `flush_queued_turn` after
                    // `Ready`, a path that never passes through `Command::Turn`. That
                    // escalation goes unannounced.
                    //
                    // Chasing call sites with broadcasts is enforced by remembering, and
                    // missing one has no symptom at all — the web shows a guard that stopped
                    // holding long ago, and a restart recovers the looser mode. So ask once on
                    // this heartbeat, which ticks anyway: `announce_mode` compares for itself
                    // and does nothing when nothing moved, so **every** escalation path is
                    // covered, including the ones added later.
                    self.announce_mode(None).await;

                    // codex writes `rollout-<ISO>-<uuid>.jsonl` under a dated
                    // directory and indexes it asynchronously, so the path is
                    // often not resolvable at the moment the thread opens.
                    // Keep trying rather than losing the permanent record.
                    if self.tailer.is_none()
                        && let Some(p) = self.driver.transcript_path() {
                            self.tailer = Some(Tailer::new(p, !self.resuming));
                        }
                    if tokio::time::Instant::now() >= self.rollover_check_at {
                        self.follow_rollover().await;
                    }
                    self.drain_transcript().await;
                    self.poll_fallback_control().await;
                }
            }
        }
    }

    async fn resolve_interrupt(
        &mut self,
        reply: Ticket<()>,
        result: crate::Result<crate::rc::harness::InterruptOutcome>,
    ) {
        if let Some(message) = result
            .as_ref()
            .err()
            .filter(|error| crate::rc::harness::is_request_id_exhaustion(error))
            .map(ToString::to_string)
        {
            self.handle_protocol_invariant(message, None, None).await;
            reply.finish(result.map(|_| ()));
            return;
        }
        let abandoned = matches!(result, Ok(crate::rc::harness::InterruptOutcome::Requested))
            && self.abandon_pending_approvals().await;
        if let Some(status) = status_after_approval_interrupt(self.info.status, abandoned) {
            self.set_status(status).await;
        }
        reply.finish(result.map(|_| ()));
    }

    async fn handle_protocol_invariant(
        &mut self,
        message: String,
        attempted_mode: Option<PermissionMode>,
        confirmation_token: Option<String>,
    ) {
        tracing_note(&format!(
            "[{}] native protocol invariant failed: {message}",
            self.info.session_id
        ));
        if let Some(pending) = self.pending_turn_command.take() {
            let attempted_mode = pending
                .guard_attempt
                .as_ref()
                .map(|attempt| attempt.expected_mode)
                .or(attempted_mode);
            self.prove_harness_tree_terminated(&message).await;
            if let Some(confirmation_token) = confirmation_token {
                self.sync_turn_guard(TurnGuardBarrier::FailClosed { confirmation_token })
                    .await;
            }
            self.resolve_turn_start_inner(
                pending,
                TurnStartOutcome::Unknown {
                    message,
                    attempted_mode,
                },
                true,
            )
            .await;
        } else {
            self.prove_harness_tree_terminated(&message).await;
            if let Some(confirmation_token) = confirmation_token {
                self.sync_turn_guard(TurnGuardBarrier::FailClosed { confirmation_token })
                    .await;
            }
            self.set_status(SessionStatus::Ended).await;
        }
    }

    #[cfg(test)]
    async fn on_harness_event(&mut self, ev: HarnessEvent) {
        self.on_harness_event_with_commands(ev, None).await;
    }

    async fn on_harness_event_with_commands(
        &mut self,
        ev: HarnessEvent,
        commands: Option<&mut mpsc::Receiver<Command>>,
    ) -> Option<Option<Command>> {
        let mut deferred_command = None;
        match ev {
            HarnessEvent::ModelUpdated => {
                self.emit(
                    method::SESSION_MODEL,
                    serde_json::json!({"session_id":self.info.session_id}),
                )
                .await;
            }
            HarnessEvent::SettingsUpdated { mode, applied } => {
                self.sync_turn_guard(TurnGuardBarrier::NativeSettings { mode })
                    .await;
                self.info.permission_mode = mode;
                self.info.dangerous |= mode.is_none_or(|mode| mode.is_dangerous());
                self.emit(
                    method::SESSION_PERMISSION_MODE,
                    SessionPermissionMode {
                        native_default: true,
                        session_id: self.info.session_id.clone(),
                        mode,
                        applied,
                        by: None,
                    },
                )
                .await;
                self.emit(
                    method::SESSION_MODEL,
                    serde_json::json!({"session_id":self.info.session_id}),
                )
                .await;
            }
            HarnessEvent::Ready {
                runtime_thread_id,
                transcript_path,
            } => {
                // The native id stays here. Viewers address the session by its
                // logical agit id, which is what makes "continue on another
                // machine" and "continue in another harness" invisible to them.
                tracing_note(&format!(
                    "session {} bound to {} thread {runtime_thread_id}",
                    self.info.session_id, self.info.runtime
                ));
                // codex's thread id exists only from this moment, and the daemon's
                // double-writer guard, the roster and the local lineage all depend on it.
                // claude-code registered at launch; doing it again here is idempotent.
                self.bind_if_known().await;
                if let Some(p) = transcript_path {
                    // Only (re)target on a *path change* (slow-path resume mints
                    // a new file). Rebuilding a tailer that already follows this
                    // path would seek to the current end of file and swallow
                    // every line written since its last poll — `init` is
                    // announced seconds after launch, and by then the transcript
                    // already holds the queue records and the user's prompt.
                    // Losing that prompt is what leaves a session nameless.
                    let same = self.tailer.as_ref().is_some_and(|t| t.path() == p);
                    if !same {
                        // Resume streams only new lines; saved history is reconciled
                        // separately by settlement and need not exist in the repo yet.
                        self.tailer = Some(Tailer::new(p, !self.resuming));
                    }
                }
                // Codex has a pre-input native Ready boundary. Claude does not:
                // its first Ready arrives only after the first user message,
                // which would deadlock a no-prompt recovery behind the daemon's
                // Drive gate. Claude therefore uses the internal command-loop
                // barrier above; Codex continues to require native evidence.
                if matches!(self.info.runtime.as_str(), "codex" | "opencode") {
                    self.sync_turn_guard(TurnGuardBarrier::Ready).await;
                }
                self.drain_transcript().await;
                // Adopting native history creates a capture branch before it contains any
                // turns. Queue settlement even if this attachment never receives new input.
                self.settlement_due |= self.resuming && self.agit_session.is_some();
                self.flush_initial_turn_if_ready().await;
            }
            HarnessEvent::InterruptResolved(result) => {
                if let Some(reply) = self.pending_interrupt_command.take() {
                    self.resolve_interrupt(reply, result.map_err(anyhow::Error::msg))
                        .await;
                }
            }
            HarnessEvent::SteerResolved(result) => {
                self.step_steer_resolved(result.is_ok()).await;
                if let Some(pending) = self.pending_steer_command.take() {
                    self.resolve_steer(pending, result.map_err(anyhow::Error::msg))
                        .await;
                }
            }
            HarnessEvent::TurnStartResolved(outcome) => {
                let Some(pending) = self.pending_turn_command.take() else {
                    tracing_note(&format!(
                        "[{}] ignored a turn/start response with no pending command",
                        self.info.session_id
                    ));
                    return None;
                };
                self.resolve_turn_start(pending, outcome).await;
            }
            HarnessEvent::TurnStartConfirmed {
                confirmation_token,
                effective_mode: _,
            } => {
                if let Some(confirmation_token) = confirmation_token {
                    self.sync_turn_guard(TurnGuardBarrier::Confirm { confirmation_token })
                        .await;
                }
            }
            HarnessEvent::TurnStarted { turn_id, prompt } => {
                self.held_turn_over(steer_queue::TurnOver::Started(&turn_id), false)
                    .await;
                self.follow_rollover().await;
                if let Err(message) = self
                    .announce_turn_started_once(turn_id, prompt, None, true)
                    .await
                {
                    self.handle_protocol_invariant(message, None, None).await;
                }
            }
            HarnessEvent::ItemStarted {
                item_id,
                kind,
                tool,
            } => {
                self.emit(
                    method::ITEM_STARTED,
                    ItemStarted {
                        item_id,
                        turn_id: String::new(),
                        kind,
                        tool,
                    },
                )
                .await;
            }
            HarnessEvent::Delta { item_id, text } => {
                // A delta goes out as the harness sent it, the moment it arrives.
                if !text.is_empty() {
                    self.emit(method::ITEM_DELTA, ItemDelta { item_id, text })
                        .await;
                }
            }
            HarnessEvent::ItemCompleted { item_id } => {
                // Stream completion is separate from the authoritative transcript record.
                self.emit("item.finished", serde_json::json!({"item_id":item_id}))
                    .await;
            }
            HarnessEvent::TurnCompleted {
                turn_id,
                outcome,
                error,
                cost_usd,
                duration_ms,
            } => {
                if let Err(message) = self
                    .announce_turn_started_once(turn_id.clone(), None, None, false)
                    .await
                {
                    self.handle_protocol_invariant(message, None, None).await;
                    return None;
                }
                if self.resolved_initial_turn.as_ref().is_some_and(|resolved| {
                    matches!(
                        &resolved.outcome,
                        TurnStartOutcome::Accepted {
                            turn_id: initial_turn_id,
                            ..
                        } if initial_turn_id == &turn_id
                    )
                }) {
                    self.resolved_initial_turn = None;
                }
                // The harness may abandon a can-use-tool request without ever
                // sending an explicit expiry (interrupt and failed turns both
                // do this). A turn boundary is authoritative: no approval from
                // that turn can still be answered.
                self.abandon_pending_approvals().await;
                // Nor can a message held for one of its tool steps still reach it.
                self.held_turn_over(
                    steer_queue::TurnOver::Ended(&turn_id),
                    matches!(outcome, TurnOutcome::Interrupted),
                )
                .await;
                // **One drain is not enough.** At the moment the harness writes `result` on
                // stdout, its last transcript line need not be persisted yet: the model's final
                // reply (`assistant_reply`) can come **after** `turn.completed`, so any client
                // that stops on turn.completed **systematically drops the most important
                // sentence of the whole turn**.
                //
                // So the end is declared only once the file goes quiet: poll until two
                // consecutive polls bring no new line, or until the cap (waiting forever is not
                // allowed — the harness may already be dead).
                self.settle_transcript().await;
                if self.settlement.borrow().local_owner
                    && let Err(error) = self.retain_completed_archive(
                        &turn_id,
                        match outcome {
                            TurnOutcome::Ok => archive_recovery::TurnEnding::Completed,
                            TurnOutcome::Interrupted if self.stop_unattended => {
                                archive_recovery::TurnEnding::StoppedByAgit
                            }
                            TurnOutcome::Interrupted => archive_recovery::TurnEnding::Interrupted,
                            TurnOutcome::Error => archive_recovery::TurnEnding::Stopped,
                        },
                    )
                {
                    self.publication_retry.failed();
                    tracing_note(&format!(
                        "completed turn archive remains unrecorded: {error:#}"
                    ));
                    self.emit("commit.failed", serde_json::json!({
                        "session_id": self.info.session_id,
                        "error": "The completed turn could not be queued for archiving. Its native history remains on this device."
                    })).await;
                }
                self.set_status(SessionStatus::Idle).await;
                let error = match error {
                    Some(message) => {
                        let report = self.redactor.scrub(&message);
                        self.alert_registered(report.registered_ids, "turn_error")
                            .await;
                        Some(report.text)
                    }
                    None => None,
                };
                let mut completion = Frame::notification(
                    method::TURN_COMPLETED,
                    TurnCompleted {
                        turn_id,
                        outcome: match outcome {
                            TurnOutcome::Ok => PTurnOutcome::Ok,
                            TurnOutcome::Interrupted => PTurnOutcome::Interrupted,
                            TurnOutcome::Error => PTurnOutcome::Error,
                        },
                        error,
                        cost_usd,
                        duration_ms,
                    },
                );
                let boundary = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
                completion.settlement_boundary = Some(boundary.clone());
                self.completed_boundary = Some(boundary);
                let _ = self.out.send(completion).await;
                // Input resumes only after settlement freezes native and code observations.
                // The owned writer and publication retain this turn's journal boundary.
                if let Some(commands) = commands {
                    deferred_command = self.settle_until_command(commands).await;
                } else {
                    self.settle_and_push(SettlementBoundary::Turn).await;
                }

                // The creation prompt can fall back into the slot **after** `Ready` — a turn
                // happened to be running at that moment and native answered "a turn is already
                // running". `Ready` comes once; the turn boundary is where that reason
                // disappears. Without a drain here, nobody ever picks the creation prompt up
                // again, and a non-empty slot turns back every later turn whose text differs:
                // the session is wedged for good.
                //
                // The drain sits only on a **real state change** like this one, never where a
                // refusal settles: `begin_turn_start` calls `resolve_turn_start` back
                // **directly** for results it can decide locally, and draining there is
                // recursing into itself.
                self.flush_initial_turn_if_ready().await;
            }
            HarnessEvent::ApprovalResolved { approval_id } => {
                let shown = self.pending.remove(&approval_id).is_some();
                if shown {
                    self.emit(
                        method::APPROVAL_RESOLVED,
                        crate::protocol::ApprovalResolved {
                            session_id: self.info.session_id.clone(),
                            approval_id,
                        },
                    )
                    .await;
                }
                if shown
                    && self.pending.is_empty()
                    && self.info.status == SessionStatus::AwaitingApproval
                {
                    self.set_status(SessionStatus::Running).await;
                }
            }
            HarnessEvent::Approval(mut req) => {
                if let Err(message) = self
                    .announce_turn_started_once(req.turn_id.clone(), None, None, true)
                    .await
                {
                    self.handle_protocol_invariant(message, None, None).await;
                    return None;
                }
                req.session_id = self.info.session_id.clone();
                // What the call reaches is judged **here**: only the machine sees the filesystem,
                // and only it can canonicalize a path and compare it against the allowlist.
                let conf = self.confinement.borrow().clone();
                let reason = crate::rc::policy::approval_owner_reason(
                    &req.tool,
                    &req.input,
                    &conf.roots,
                    // A relative path resolves against **this session's** cwd: agitd is a
                    // daemon, its own process cwd has nothing to do with the session, and a
                    // comparison computed from that base is wrong.
                    &self.cwd,
                    &conf.operator_heads,
                )
                .or_else(|| {
                    (!self.driver.permission_mode_known())
                        .then_some(crate::protocol::OwnerReason::Unprovable)
                });
                // Anyone who may answer approvals answers this one; the reason only says what it
                // reaches, for whoever reads the card.
                req.requires_owner = false;
                req.owner_reason = reason;
                let authority = PendingApproval {
                    suggested_permission_mode: req.suggested_permission_mode,
                };
                self.set_status(SessionStatus::AwaitingApproval).await;
                // The card is the request as the harness raised it, and it is answerable from the
                // moment it goes out. A harness can deliver the same request again; its newer
                // copy replaces the pending one and is shown again.
                self.pending.insert(req.approval_id.clone(), authority);
                self.emit(method::APPROVAL_REQUEST, req).await;
            }
            HarnessEvent::Notice { text } => {
                tracing_note(&format!("[{}] {text}", self.info.session_id));
            }
            HarnessEvent::GoalUpdated { goal } => {
                self.emit(
                    "session.goal",
                    serde_json::json!({"goal":self.redactor.scrub_json(&goal).value}),
                )
                .await;
            }
            HarnessEvent::Progress { text } => {
                self.emit("session.progress", serde_json::json!({"text":text}))
                    .await;
            }
            HarnessEvent::ProtocolInvariant {
                message,
                attempted_mode,
                confirmation_token,
            } => {
                self.handle_protocol_invariant(message, attempted_mode, confirmation_token)
                    .await;
            }
            HarnessEvent::StepSteerDelivered { operation_id } => {
                self.step_steer_delivered(operation_id);
            }
            HarnessEvent::StepSteerUndelivered {
                operation_id,
                interrupted,
            } => {
                self.step_steer_undelivered(operation_id, interrupted).await;
            }
            HarnessEvent::Exited { .. } => {}
        }
        deferred_command
    }

    fn idle_settlement_ready(&self) -> bool {
        (self.settlement_due || self.publication_retry.due())
            && self.local_settlement.is_none()
            && self.publication.is_none()
            && self.info.status == SessionStatus::Idle
            && self.pending_turn_command.is_none()
            && self.pending_steer_command.is_none()
            && settlement_lease(&self.settlement).is_some()
    }

    /// Ordinary controls leave repository transactions intact; publication cannot block them.
    async fn settle_until_command(
        &mut self,
        commands: &mut mpsc::Receiver<Command>,
    ) -> Option<Option<Command>> {
        self.finish_local_settlement(false).await;
        self.finish_publication(false).await;
        if self.local_settlement.is_some() || self.publication.is_some() {
            self.settlement_due = true;
            return None;
        }
        self.settlement_due = false;
        let retrying = self.publication_retry.next.take().is_some();
        let boundary = self.completed_boundary.clone();
        let deferred = self
            .settle_and_push_inner(SettlementBoundary::Turn, boundary, Some(commands))
            .await;
        if retrying {
            self.retry_unfinished_publication();
        }
        deferred
    }

    fn retry_unfinished_publication(&mut self) {
        if self.publication.is_none()
            && self.local_settlement.is_none()
            && self.publication_retry.failures > 0
            && self.publication_retry.next.is_none()
        {
            self.publication_retry.failed();
        }
    }

    async fn finish_publication(&mut self, wait: bool) {
        if !self
            .publication
            .as_ref()
            .is_some_and(|task| wait || task.0.is_finished())
        {
            return;
        }
        let mut task = self.publication.take().expect("publication is present");
        match (&mut task.0).await {
            Ok(outcome) => self.finish_publication_outcome(outcome),
            Err(_) => self.publication_retry.failed(),
        }
    }

    fn finish_publication_outcome(&mut self, outcome: PublicationOutcome) {
        match outcome {
            PublicationOutcome::Complete(delivery) => {
                let acknowledged = delivery.as_ref().is_none_or(|delivery| {
                    delivery.status() == crate::protocol::DeliveryStatus::Delivered
                });
                if let Some(delivery) = delivery {
                    self.apply_publication_delivery(delivery);
                }
                if acknowledged {
                    self.retry_while_a_turn_waits();
                } else {
                    self.publication_retry.failed();
                }
            }
            PublicationOutcome::Retry => self.publication_retry.failed(),
        }
    }

    fn apply_publication_delivery(&mut self, delivery: Arc<crate::protocol::ConnectionDelivery>) {
        match delivery.status() {
            crate::protocol::DeliveryStatus::Delivered => self.settlement_acked(),
            crate::protocol::DeliveryStatus::Pending => {
                if let Some(pending) = self.pending_settlement.as_mut() {
                    pending.delivery = Some(delivery);
                }
            }
            crate::protocol::DeliveryStatus::Stale => {}
        }
    }

    /// The last transcript drain and settlement before the session closes.
    ///
    /// Skip it and the lines the harness wrote into the transcript after the final turn
    /// boundary never become `item.completed`; worse, `pending_settlement` lives only in memory
    /// — after a failed push, the user stopping the session (or the harness exiting on its own)
    /// leaves that commit with nobody to push it and `commit.settled` is never sent: the local
    /// repo plainly holds the final turn while on the hub the session looks as if it was never
    /// persisted. Called **after** `driver.shutdown()`: the process tree is already reaped, the
    /// transcript takes no more appends, and this drain reads everything.
    async fn settle_on_exit(&mut self) {
        self.finish_local_settlement(true).await;
        self.finish_publication(true).await;
        self.drain_transcript().await;
        // A turn the reaped harness was still running when it ended on its own ends here, as a
        // turn of its own; otherwise the exit commit finds no completion and the turn waits for one
        // that may never come. A turn cut off by a person's stop is held like their interrupt; one
        // a stop nobody chose cut ends where it stopped.
        let ending = if self.stop_unattended {
            archive_recovery::TurnEnding::StoppedByAgit
        } else {
            archive_recovery::TurnEnding::Stopped
        };
        if let AnyDriver::ClaudeCode(driver) = &self.driver
            && let Some(turn_id) = driver.current_turn()
            && (!self.stop_requested || self.stop_unattended)
            && self.settlement.borrow().local_owner
            && let Err(error) = self.retain_completed_archive(turn_id, ending)
        {
            tracing_note(&format!(
                "[{}] stopped turn archive remains unrecorded: {error:#}",
                self.info.session_id
            ));
        }
        self.settle_and_push(SettlementBoundary::SessionExit).await;
    }

    /// The hub really took that `commit.settled`: the in-memory pending entry and the on-disk
    /// receipt are destroyed together. **Only `Delivered` reaches here** — Pending and Stale are
    /// not confirmations, and the receipt has to outlive this process.
    fn settlement_acked(&mut self) {
        if let Some(receipt) = self
            .pending_settlement
            .take()
            .and_then(|pending| pending.receipt)
        {
            // A failed delete only makes the next turn send one duplicate `commit.settled`
            // (idempotent by sha), which beats keeping a notification that really never
            // arrived and never sending it.
            let _ = std::fs::remove_file(receipt);
        }
    }

    /// Once the settlement is persisted, push this branch to the project's private repo and
    /// report the commit to the hub.
    ///
    /// Failure is **not an error**: the network may be down, the user may not be signed in. The
    /// session keeps running and the next turn tries again; git on the machine is still the
    /// truth, and the push only copies it to the hub. What is unacceptable is the reverse —
    /// interrupting a session that is doing work because a push failed.
    async fn settle_and_push(&mut self, boundary: SettlementBoundary) {
        self.finish_local_settlement(true).await;
        self.finish_publication(true).await;
        let _ = self.settle_and_push_inner(boundary, None, None).await;
    }

    async fn settle_and_push_inner(
        &mut self,
        boundary: SettlementBoundary,
        journal_boundary: Option<Arc<std::sync::atomic::AtomicU64>>,
        commands: Option<&mut mpsc::Receiver<Command>>,
    ) -> Option<Option<Command>> {
        let mut deferred_command = None;
        let Some(lease) = settlement_lease(&self.settlement) else {
            return deferred_command;
        };
        if let Some(delivery) = self
            .pending_settlement
            .as_ref()
            .and_then(|pending| pending.delivery.clone())
        {
            let status = if delivery.epoch() == lease.epoch && journal_boundary.is_some() {
                delivery.status()
            } else if delivery.epoch() == lease.epoch {
                wait_for_connection_delivery_within(
                    &mut self.settlement,
                    lease,
                    &delivery,
                    SETTLEMENT_DELIVERY_WAIT,
                )
                .await
            } else {
                delivery.invalidate();
                delivery.status()
            };
            match status {
                crate::protocol::DeliveryStatus::Delivered => {
                    self.settlement_acked();
                }
                crate::protocol::DeliveryStatus::Stale => {
                    if let Some(pending) = self.pending_settlement.as_mut() {
                        pending.delivery = None;
                    }
                }
                // The preceding `commit.settled` is still queued outbound (the connection is
                // alive and the epoch unchanged, it is merely behind others). Yielding is fine
                // on a turn boundary — the next turn comes through here again; but **a session
                // exit has no next turn**: returning here leaves the final turn that
                // `drain_transcript` just produced with no commit, no push and no
                // `commit.settled` ever, while `SessionNote::Ended` goes out anyway and the
                // session on the hub stops one turn back yet shows as ended — exactly the hole
                // `settle_on_exit` plugs.
                crate::protocol::DeliveryStatus::Pending => {
                    if boundary == SettlementBoundary::Turn {
                        self.settlement_due = journal_boundary.is_some();
                        return deferred_command;
                    }
                }
            }
        }
        // Try again when where it lands has not been built for **the current thread id**. This
        // is landing's retry entry point: the first attempt can fail transiently on the network
        // (the clone step) or on disk, and the settlement is about to need it.
        //
        // The test cannot be `landed_thread.is_none()`. A failed `land()` leaves that field
        // alone, so it stays on the **preceding** thread id — while claude's slow-path recovery
        // mints a new id at `system/init` (and a new transcript file with it). In that case
        // `is_none()` is always false and no retry ever happens, which is exactly the case that
        // needs one most: the store link still points at the transcript nobody appends to, and
        // every later turn settles nothing.
        let Some(thread_id) = self.driver.runtime_thread_id() else {
            return deferred_command;
        };
        // A conversation the folder it runs in keeps back gets no branch, commit or upload from
        // RC, whoever drives it.
        if !crate::rc::capture::consented(
            &self.cwd,
            &self.info.runtime,
            &thread_id,
            self.info.project_id.is_some(),
        ) {
            return deferred_command;
        }
        if self.landed_thread.as_deref() != Some(thread_id.as_str()) {
            self.announce_binding().await;
        }
        // Re-run land even after a prior success: it resolves the current slug
        // and compares the immutable ID before this turn's local commit.
        self.land(&thread_id, lease).await;
        // Landing proves that the current slug still resolves to the immutable
        // ID and that this checkout carries the same repo-local pin. A failed
        // or cancelled proof must stop before the local commit, not merely
        // leave the later push to reject a reused slug.
        if self.landed_thread.as_deref() != Some(thread_id.as_str())
            || !settlement_lease_is_current(&self.settlement, lease)
        {
            if self.agit_session.is_some() {
                self.publication_retry.failed();
            }
            return deferred_command;
        }
        let Some(agit_session) = self.agit_session.clone() else {
            return deferred_command; // this session is unmanaged (no project bound), so there is nowhere to push
        };
        let slug = agit_session.slug();
        let branch = agit_session.branch().to_string();
        let expected_agent_id = agit_session.agent_id().to_string();
        // Lineage computes the repo path itself. Doing `slug.split('/')` here and feeding
        // `repo_dir` is the vine that climbs out of the repo root with `../..`, three files away
        // from the point that validates it.
        let Some(repo_dir) = self.settlement_repo_dir(&agit_session) else {
            return deferred_command;
        };
        // The durable watermark on the notification side. See [`unacked_settlement_path`]: it
        // asks something **different** from git reachability.
        let receipt_path = unacked_settlement_path(&repo_dir, &branch);

        let Some(exe) = self.settlement_exe() else {
            return deferred_command;
        };
        let cwd = self.cwd.clone();
        let agit_session_env = agit_session.to_string();
        let command = |args: &[&str]| {
            let mut command = exe.command();
            command
                .args(args)
                .current_dir(&cwd)
                // These three values are one capability. `AGIT_SESSION` gives
                // the route; the immutable ID fences the checkout and every
                // HTTP/Git mutation; the supervisor alone owns RC settlement.
                .env("AGIT_SESSION", &agit_session_env)
                .env(
                    crate::hub::identity::EXPECTED_AGENT_ID_ENV,
                    &expected_agent_id,
                )
                // The daemon itself may have been launched from inside an RC
                // session and inherited that outer harness marker. Its own
                // fenced subprocess is the supervisor writer, not a Stop hook.
                .env_remove(crate::rc::harness::SUPERVISED_HOOK_ENV);
            if lease.local_owner {
                command
                    .env_remove(crate::hub::identity::EXPECTED_AGENT_ID_ENV)
                    .env("AGIT_LOCAL_AGENT_ID", &expected_agent_id);
                if let Some(kind) = &agit_session.capture {
                    command.env(
                        crate::rc::capture::CAPTURE_ENV,
                        serde_json::to_string(kind).expect("capture kind serializes"),
                    );
                }
            }
            command
        };

        // The watermark reads **this session's branch** ref, not the main checkout's HEAD: the
        // settlement lands in the branch's own worktree while the main checkout stays on main,
        // so HEAD is identical before and after while the branch has moved on.
        let watermark_ref = settlement_watermark_ref(&branch);

        let repo_dir_s = repo_dir.to_string_lossy().into_owned();
        // RC Stop hooks deliberately no-op; this cancellable path is the only
        // settlement writer. Losing the negotiated feature kills this process
        // (and its git process group) instead of letting a static child env keep
        // committing after authorization disappeared.
        let Ok(result_file) = tempfile::NamedTempFile::new_in(repo_dir.join(".git")) else {
            return deferred_command;
        };
        let Ok(prepared_file) = tempfile::NamedTempFile::new_in(repo_dir.join(".git")) else {
            return deferred_command;
        };
        let claim = self
            .info
            .native_source
            .as_ref()
            .map(|source| source.session_ref(&thread_id))
            .unwrap_or_else(|| thread_id.clone());
        let mut strict_commit = command(&["commit", "--from-supervisor"]);
        strict_commit
            .env(crate::commands::commit::SUPERVISOR_RESULT_ENV, result_file.path())
            .env(crate::commands::commit::SUPERVISOR_PREPARED_ENV, prepared_file.path())
            .env(
                crate::commands::commit::archive::NATIVE_ENV,
                serde_json::json!({
                    "runtime": crate::adapter::normalize(&self.info.runtime).map(str::to_owned).unwrap_or_else(|_| self.info.runtime.clone()),
                    "session_id": claim,
                })
                .to_string(),
            );
        strict_commit.env_remove(crate::commands::commit::archive::ROLE_ENV);
        if let Some(handoff) = &self.archive_handoff {
            let Ok(role) = serde_json::to_string(&handoff.role) else {
                return deferred_command;
            };
            strict_commit.env(crate::commands::commit::archive::ROLE_ENV, role);
        }
        let request = settlement_io::LocalCommitRequest {
            session_id: self.info.session_id.clone(),
            settlement: self.settlement.clone(),
            lease,
            watermark: settlement_io::BranchWatermark {
                repository: repo_dir.clone(),
                reference: watermark_ref,
            },
            commit: strict_commit,
            result_file,
            prepared_file,
        };
        let mut push = command(&[]);
        crate::commands::auto_push::automatic_push(push.as_std_mut(), &agit_session_env);
        let context = settlement_io::LocalSettlementContext {
            lease,
            receipt_path,
            repo_dir_s,
            slug,
            branch,
            expected_agent_id,
            journal_boundary,
            push,
            claim,
        };
        self.local_commit_with_reads(request, context, commands, &mut deferred_command)
            .await;
        deferred_command
    }

    async fn finish_local_commit(
        &mut self,
        context: settlement_io::LocalSettlementContext,
        result: settlement_io::LocalCommit,
    ) {
        let settlement_io::LocalSettlementContext {
            lease,
            receipt_path,
            repo_dir_s,
            slug,
            branch,
            expected_agent_id,
            journal_boundary,
            mut push,
            claim,
        } = context;
        if !settlement_lease_is_current(&self.settlement, lease) {
            return;
        }
        self.privacy_claim = Some(claim);
        let settlement_io::LocalCommit {
            before,
            output: commit,
            after,
            reported,
        } = result;
        let pending = match self
            .pending_settlement
            .as_ref()
            .map(|pending| pending.sha.clone())
        {
            Some(sha) => Some(sha),
            // A no-op can retry an undelivered notification or an unpublished source commit.
            // Publication and notification receipts establish different completion boundaries.
            None if reported.is_none() && after == before => {
                // The receipt counts only while it points at the current HEAD: a HEAD that has
                // moved on means a later settlement followed, whose `commit.settled` covers
                // this one, and accepting a sha that does not match here only makes the
                // predicate say "HEAD moved away" and stops the whole turn.
                match read_unacked_settlement(&receipt_path).filter(|sha| *sha == after) {
                    Some(sha) => Some(sha),
                    None => {
                        unpushed_local_head(
                            &mut self.settlement,
                            lease,
                            &repo_dir_s,
                            &after,
                            &branch,
                            &expected_agent_id,
                        )
                        .await
                    }
                }
            }
            None => None,
        };
        let candidate = match strict_settlement_candidate(
            &before,
            &commit,
            &after,
            reported.as_deref(),
            pending.as_deref(),
        ) {
            Ok(candidate) => candidate,
            Err(reason) => {
                // A policy refusal (content that does not continue the branch, a blocked secret,
                // a capture over its budget) repeats until the transcript or configuration
                // changes.
                if commit.status.code() == Some(crate::ExitCode::Policy.as_i32()) {
                    self.publication_retry.refused();
                } else {
                    self.publication_retry.failed();
                }
                if lease.local_owner {
                    let detail =
                        settlement_failure_detail(reason, &String::from_utf8_lossy(&commit.stderr));
                    let scrubbed = self.redactor.scrub(&detail);
                    if self.reported_settlement_failure.as_deref() != Some(scrubbed.text.as_str()) {
                        self.emit("commit.failed", serde_json::json!({"session_id":self.info.session_id,"error":scrubbed.text})).await;
                        self.reported_settlement_failure = Some(scrubbed.text);
                    }
                }
                tracing_note(&format!(
                    "strict RC settlement stopped ({reason}): {}",
                    String::from_utf8_lossy(&commit.stderr).trim()
                ));
                return;
            }
        };
        self.reported_settlement_failure = None;
        let Some(sha) = candidate else {
            // The commit succeeded and settled no new turn: either nothing was left, or it
            // stopped at the privacy check, which it recorded.
            self.retry_while_a_turn_waits();
            return;
        };
        if lease.local_owner {
            let mut notification = Frame::notification(
                method::COMMIT_LOCAL_SETTLED,
                crate::protocol::CommitLocalSettled {
                    session_id: self.info.session_id.clone(),
                    through_seq: 0,
                },
            );
            notification.settlement_boundary = journal_boundary.clone();
            self.pending_settlement = None;
            let _ = self.out.send(notification).await;
            self.publish_local_source(
                lease,
                &repo_dir_s,
                &branch,
                &expected_agent_id,
                &sha,
                push,
                journal_boundary,
            )
            .await;
            return;
        }
        // Persist the source before publication so a crash cannot lose its notification retry.
        // The remote notification receives the generated public SHA only after a matching result.
        if !lease.local_owner {
            record_unacked_settlement(&receipt_path, &sha, &branch);
            self.pending_settlement = Some(PendingSettlement {
                sha: sha.clone(),
                delivery: None,
                receipt: Some(receipt_path),
            });
        }

        let mut notification = Frame::notification(
            method::COMMIT_SETTLED,
            CommitSettled {
                session_id: self.info.session_id.clone(),
                agent: Some(slug.clone()),
                expected_agent_id: Some(expected_agent_id.clone()),
                branch: Some(branch.clone()),
                commit_sha: sha.clone(),
                // The daemon resolves the captured boundary in its own journal coordinates.
                through_seq: 0,
                turns: None,
            },
        );
        notification.settlement_boundary = journal_boundary.clone();
        let record = self.upload_place(&slug, &branch);
        let publication = (|| -> crate::Result<_> {
            use std::io::Write;
            let repo = crate::domain::repo::Repo::at(std::path::Path::new(&repo_dir_s));
            let destination =
                crate::hub::identity::require_current(&repo, &crate::infra::config::hub_url())?;
            anyhow::ensure!(
                destination.agent_id == expected_agent_id,
                "RC publication destination changed"
            );
            let request = SupervisorPushRequest {
                version: 1,
                request_id: uuid::Uuid::now_v7().to_string(),
                repository: slug,
                branch,
                source: sha,
                destination,
                notification_id: None,
            };
            request.validate()?;
            let mut file = tempfile::NamedTempFile::new_in(repo.common_dir()?)?;
            file.write_all(&serde_json::to_vec(&request)?)?;
            Ok((request, file))
        })();
        let (request, result_file) = match publication {
            Ok(value) => value,
            Err(error) => {
                tracing_note(&format!("cannot prepare RC publication: {error:#}"));
                return;
            }
        };
        local_publication::direct(&mut push, &request, result_file.path());
        let publish = publish_settlement(
            self.settlement.clone(),
            lease,
            push,
            request,
            result_file,
            notification,
            self.out.clone(),
            record,
        );
        if journal_boundary.is_some() {
            self.publication = Some(Publication(tokio::spawn(async move {
                match publish.await {
                    Some(delivery) => PublicationOutcome::Complete(Some(delivery)),
                    None => PublicationOutcome::Retry,
                }
            })));
        } else {
            let outcome = match publish.await {
                Some(delivery) => PublicationOutcome::Complete(Some(delivery)),
                None => PublicationOutcome::Retry,
            };
            self.finish_publication_outcome(outcome);
        }
    }

    /// Wait for the transcript file to go quiet.
    ///
    /// The cap is [`SETTLE_MAX_MS`]: declaring a turn over late is fine, never declaring it is
    /// not.
    async fn settle_transcript(&mut self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(SETTLE_MAX_MS);
        let mut quiet_rounds = 0;
        while std::time::Instant::now() < deadline && quiet_rounds < 2 {
            let before = self.consumed_bytes;
            self.drain_transcript().await;
            if self.consumed_bytes == before {
                quiet_rounds += 1;
            } else {
                quiet_rounds = 0;
            }
            tokio::time::sleep(std::time::Duration::from_millis(TAIL_POLL_MS)).await;
        }
    }

    /// Move the tailer into the segment Codex continues a long thread in.
    ///
    /// Codex rolls a thread over into a new carrier at a turn start and its index then names
    /// that carrier. A tailer left on the previous one stops seeing completed items, and archive
    /// boundaries keep pointing into it. The tailer moves only to a carrier whose header
    /// continues the one it reads, after draining it, and reads the new one from its start:
    /// its records and coordinates are that carrier's own, as history pages report them.
    async fn follow_rollover(&mut self) {
        self.rollover_check_at = tokio::time::Instant::now() + ROLLOVER_CHECK;
        if self.info.runtime != "codex" {
            return;
        }
        let Some(current) = self.tailer.as_ref().map(|tailer| tailer.path().to_owned()) else {
            return;
        };
        let Some(next) = self.driver.transcript_path() else {
            return;
        };
        if next == current || !crate::adapter::codex_rollout::continues(&next, &current) {
            return;
        }
        self.drain_transcript().await;
        tracing_note(&format!(
            "[{}] following the native transcript into its next segment",
            self.info.session_id
        ));
        self.tailer = Some(Tailer::new(next, true));
        self.drain_transcript().await;
    }

    /// Read new transcript lines and emit one `item.completed` per IR event.
    ///
    /// The parse is per-line and stateless, which works because the IR's
    /// `Event::line` coordinate is a physical line number: a line's events do
    /// not depend on the lines before it.
    async fn drain_transcript(&mut self) {
        if let Some(snapshot) = self.driver.take_native_snapshot() {
            let projection = snapshot
                .bytes
                .map_err(anyhow::Error::msg)
                .and_then(|bytes| {
                    if snapshot.finalized {
                        self.native_records.project_final(&bytes, self.resuming)
                    } else {
                        self.native_records.project(&bytes, self.resuming)
                    }
                });
            match projection {
                Ok(items) => {
                    self.consumed_bytes = self.consumed_bytes.saturating_add(1);
                    for item in items {
                        self.emit(method::ITEM_COMPLETED, item).await;
                    }
                }
                Err(error) => {
                    tracing_note(&format!("Native transcript projection failed: {error}"));
                    self.emit(method::TURN_COMPLETED, TurnCompleted {
                        turn_id: String::new(),
                        outcome: PTurnOutcome::Error,
                        error: Some("OpenCode's native history could not be displayed. Resume the session to recover its stored records.".into()),
                        cost_usd: None,
                        duration_ms: None,
                    }).await;
                }
            }
            return;
        }
        let Some(tailer) = self.tailer.as_mut() else {
            return;
        };
        let (mode, polled) = if self.info.runtime == "codex" {
            let batch = match tailer.poll_codex() {
                Ok(batch) => {
                    self.transcript_readable = true;
                    batch
                }
                Err(_) => {
                    self.transcript_readable = false;
                    return;
                }
            };
            self.consumed_bytes = tailer.consumed();
            (batch.mode, batch.lines)
        } else {
            (
                super::codex_history::HistoryMode::Model,
                poll_and_mark(tailer, &mut self.consumed_bytes),
            )
        };
        if polled.is_empty() {
            return;
        }

        // Build every item first, then emit. `Box<dyn Adapter>` is not `Send`,
        // so it must not be alive across an `.await` — keeping the parse in its
        // own scope (inside `items_from_lines`) is what lets this whole session
        // run as a spawned task.
        let mut items = items_from_lines_with_mode(&self.info.runtime, &polled, mode);
        self.name_absorbed_steers(&mut items);
        for item in items {
            self.emit(method::ITEM_COMPLETED, item).await;
        }
    }

    /// Whether one more steer may go to headless Claude Code. Each one waiting is kept until its
    /// copy in the transcript is named; one past [`MAX_ABSORBING_STEERS`] is refused before it is
    /// written, rather than forgetting one that waits.
    fn admits_absorbing_steer(&self) -> crate::Result<()> {
        if self.info.runtime == "claude-code" && self.absorbing_steers.len() >= MAX_ABSORBING_STEERS
        {
            return Err(crate::rc::harness::TooManySteers.into());
        }
        Ok(())
    }

    /// Remembers a steer headless Claude Code took for the running turn's next tool step, until
    /// its copy in the transcript is named. A steer past [`MAX_ABSORBING_STEERS`] is refused
    /// before it is written, so none is ever forgotten here while it waits.
    fn absorbing(&mut self, native_prompt_id: String, message: String) {
        self.absorbing_steers.push_back(AbsorbingSteer {
            native_prompt_id,
            message,
            written_in: self.turns_started,
        });
    }

    /// agit started a turn on headless Claude Code with this prompt id: the turn takes the next
    /// number. Only the most recent ids are kept. Forgetting the oldest loses only prompts that
    /// come right after a steer's turn, never the steer's own number, and the newer prompts kept
    /// are later still, so a waiting steer is let go by whichever of them is read.
    fn started_prompt(&mut self, prompt: String) {
        if self.info.runtime != "claude-code" {
            return;
        }
        self.turns_started += 1;
        if self.started_prompts.len() >= MAX_STARTED_PROMPTS {
            self.started_prompts.pop_front();
        }
        self.started_prompts.push_back((prompt, self.turns_started));
    }

    /// Whether the prompt record `read` is of a turn agit started after `steer` was written:
    /// such a turn's prompt comes after the steer's copy in the transcript, so the copy can no
    /// longer come. A prompt of the steer's own turn or an earlier one, however late it is read,
    /// says nothing; nor does one agit didn't write, or one too old to be kept.
    fn starts_a_later_turn(&self, steer: &AbsorbingSteer, read: &str) -> bool {
        self.started_prompts
            .iter()
            .find(|(prompt, _)| prompt == read)
            .is_some_and(|(_, turn)| *turn > steer.written_in)
    }

    /// Names the transcript's copy of each steer headless Claude Code took into a running turn.
    ///
    /// A steer that reaches the turn at a tool step is recorded as `queue-operation remove`
    /// with reason `absorbed_mid_turn`: the words, with no record id. Without a name the copy
    /// can't be matched to the message the steer announced, so viewers show the words twice and
    /// the announced one stays waiting for a tool step that already came. A steer that starts
    /// the next turn instead comes back as a user record carrying its own id, and is forgotten
    /// here. Steers are matched in the order they were written, by their exact words.
    ///
    /// Claude Code appends a turn's records in order, and a steer's copy belongs to the turn it
    /// was written in, so it comes before the prompt record of any turn agit started after it.
    /// A steer is forgotten unnamed only once such a prompt is read, known by the id agit gave
    /// it; prompts of its own turn or earlier ones, however late they are read, don't count.
    /// Nothing here depends on when a turn ended or how long a read took. If the transcript never
    /// yields those records, the steers waiting here go when the session ends, and the limit
    /// refuses new steers until then.
    fn name_absorbed_steers(&mut self, items: &mut [ItemCompleted]) {
        if self.info.runtime != "claude-code" {
            return;
        }
        for item in items.iter_mut() {
            if let Some(uuid) = item.raw["uuid"].as_str()
                && item.raw["type"] == "user"
            {
                let prompt = item.event.kind == crate::adapter::EventKind::UserPrompt;
                let later: Vec<bool> = self
                    .absorbing_steers
                    .iter()
                    .map(|steer| prompt && self.starts_a_later_turn(steer, uuid))
                    .collect();
                let mut later = later.into_iter();
                self.absorbing_steers.retain(|steer| {
                    let later = later.next().unwrap_or(false);
                    steer.native_prompt_id != uuid && !later
                });
                continue;
            }
            if item.native_prompt_id.is_some()
                || item.event.kind != crate::adapter::EventKind::UserInterjection
                || item.raw["type"] != "queue-operation"
                || item.raw["operation"] != "remove"
                || item.raw["reason"] != "absorbed_mid_turn"
            {
                continue;
            }
            let Some(words) = item.raw["content"].as_str() else {
                continue;
            };
            if let Some(index) = self
                .absorbing_steers
                .iter()
                .position(|steer| steer.message == words)
            {
                let steer = self.absorbing_steers.remove(index).expect("found above");
                item.native_prompt_id = Some(steer.native_prompt_id);
            }
        }
    }
}

/// A steer headless Claude Code took for the running turn's next tool step, by its words and the
/// native id its transcript copy is to be named by.
struct AbsorbingSteer {
    native_prompt_id: String,
    message: String,
    /// The number of the turn agit had last started when it was written, which its copy belongs
    /// to; 0 if it had started none.
    written_in: u64,
}

/// How many started prompts' ids are remembered. A steer's own turn is kept as a number, so
/// forgetting older ids never strands it.
const MAX_STARTED_PROMPTS: usize = 64;

/// How many such steers may wait at once; one more is refused unsent.
const MAX_ABSORBING_STEERS: usize = 16;

/// Transcript lines → `item.completed` payloads. Shared by the supervised session, the read-only
/// watch path and history pages: each must produce the exact same items for the same lines, or
/// the views of one session drift.
pub fn items_from_lines(
    runtime: &str,
    lines: &[crate::rc::tail::TailedLine],
) -> Vec<ItemCompleted> {
    items_from_lines_with_mode(runtime, lines, super::codex_history::HistoryMode::Model)
}

/// [`items_from_lines`] for lines read in `mode`. A record goes as the harness wrote it: its
/// `raw` is the line's own JSON, capped for the wire, and its `object_hash` is the hash the
/// committed envelope carries, which is what lets the hub reconcile the live projection against
/// the pushed transcript.
pub(crate) fn items_from_lines_with_mode(
    runtime: &str,
    lines: &[crate::rc::tail::TailedLine],
    mode: super::codex_history::HistoryMode,
) -> Vec<ItemCompleted> {
    let Ok(adapter) = crate::adapter::get(runtime) else {
        return Vec::new();
    };
    let mut out = vec![];
    for line in lines {
        if line.text.trim().is_empty() {
            continue;
        }
        let (raw, events) = if runtime == "codex" {
            let Ok(raw) = serde_json::from_str::<serde_json::Value>(&line.text) else {
                continue;
            };
            let events = super::codex_history::events(&raw, mode);
            (raw, events)
        } else {
            let Ok(parsed) = adapter.parse(&line.text) else {
                continue;
            };
            let Ok(raw) = serde_json::from_str::<serde_json::Value>(&line.text) else {
                continue;
            };
            (raw, parsed.events)
        };
        if events.is_empty() {
            continue;
        }
        let native_prompt_id = (runtime == "codex")
            .then(|| super::codex_history::prompt_identity(&raw, mode))
            .flatten();
        let object_hash = transcript::object_hash(&raw);
        let (raw, truncated) = cap_raw(raw);
        for (i, mut event) in events.into_iter().enumerate() {
            event.line = Some(line.lineno as usize);
            out.push(ItemCompleted {
                source_id: line
                    .source
                    .as_ref()
                    .map(|source| format!("{source}:{object_hash}#{i}")),
                item_id: format!("{}#{i}", line.lineno),
                turn_id: String::new(),
                native_prompt_id: native_prompt_id.clone(),
                event,
                line: line.lineno,
                object_hash: object_hash.clone(),
                // Only the first event of a line carries the raw copy —
                // one line can produce several events and shipping the
                // same bytes N times is pure waste.
                raw: if i == 0 {
                    raw.clone()
                } else {
                    serde_json::Value::Null
                },
                raw_truncated: truncated,
            });
        }
    }
    out
}

/// Cap an oversized raw line. Returns the value to send and whether it was cut.
fn cap_raw(v: serde_json::Value) -> (serde_json::Value, bool) {
    let s = serde_json::to_string(&v).unwrap_or_default();
    if s.len() <= RAW_LINE_CAP {
        return (v, false);
    }
    let mut capped = serde_json::json!({
        "_truncated": true,
        "_bytes": s.len(),
        "_note": "full line is in the committed transcript; open the session to read it"
    });
    // Native identities must survive payload caps so pages reconcile with live rows.
    for key in ["type", "uuid"] {
        if let Some(value) = v[key].as_str().filter(|value| value.len() <= 128) {
            capped[key] = serde_json::json!(value);
        }
    }
    if let Some(ordinal) = v["ordinal"].as_u64() {
        capped["ordinal"] = serde_json::json!(ordinal);
    }
    // Replacement history can dwarf the summary that identifies a compaction reply.
    if v["type"] == "compacted" {
        let payload = serde_json::json!({"message":v["payload"]["message"]});
        if serde_json::to_vec(&payload).is_ok_and(|bytes| bytes.len() < RAW_LINE_CAP / 2) {
            capped["payload"] = payload;
        }
    }
    (capped, true)
}

/// The daemon logs to stderr; this crate has no tracing dependency and the CLI's
/// `ui` module is behind the `cli` feature, so keep it plain.
fn tracing_note(msg: &str) {
    eprintln!("agitd: {msg}");
}

fn trace_phase(session_id: &str, phase: &str, started: std::time::Instant) {
    tracing_note(
        &serde_json::json!({
            "event": "session.phase", "phase": phase,
            "time": chrono::Utc::now().to_rfc3339(), "session_id": session_id,
            "elapsed_ms": started.elapsed().as_secs_f64() * 1000.0,
        })
        .to_string(),
    );
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod settle_tests;
