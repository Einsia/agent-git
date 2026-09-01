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
//! # Redaction happens here, before anything leaves the machine
//!
//! Every event is scrubbed with a `domain::redact::Redactor` carrying this
//! machine's persona plus the daemon's device-local secret filter — the same
//! path `agit export` uses for publishing a copy. That masks secrets (the whole
//! gitleaks rule set plus the registered low-entropy literals), the home
//! directory, the hostname and public IPs.
//! Doing it at the edge rather than at the hub means a leaked key never reaches
//! the network at all.
//!
//! A registered literal is the one thing whose removal also changes the line's
//! advertised identity: keeping the original hash would hand the hub an offline
//! oracle for guessing a low-entropy value. See [`projected_object_hash`].

use crate::domain::{redact, transcript};
use crate::protocol::{
    ApprovalResponse, CommitSettled, Delivery, Frame, ItemCompleted, ItemDelta, ItemStarted,
    PermissionApply, PermissionMode, RAW_LINE_CAP, SecretDetected, SessionInfo,
    SessionPermissionMode, SessionStatus, TurnCompleted, TurnOutcome as PTurnOutcome, TurnSource,
    TurnStarted, method,
};
use crate::rc::harness::{
    AnyDriver, ApprovalOutcome, BoundedTurnIds, HarnessEvent, LaunchSpec,
    PermissionModeChangeError, PermissionModeOutcome, TurnGuardAttempt, TurnOutcome,
    TurnStartDispatch, TurnStartOutcome,
};
use crate::rc::tail::Tailer;
use crate::rc::ticket::Ticket;
use std::path::PathBuf;
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
    state.agent_identity_v1.then_some(state)
}

fn settlement_lease_is_current(
    state: &tokio::sync::watch::Receiver<SettlementState>,
    lease: SettlementState,
) -> bool {
    *state.borrow() == lease && lease.agent_identity_v1
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

/// The settlement whose `commit.settled` the hub has not confirmed — a watermark on disk.
///
/// # Why git reachability cannot be this watermark
///
/// [`unpushed_local_head`] derives whether the git side at the hub took this commit, reading the
/// remote-tracking ref `git push` maintains itself. **Whether the notification arrived is a
/// second watermark**: the remote ref advances as soon as the push succeeds, while
/// `commit.settled` is still queued outbound and may reach the hub much later — or never.
///
/// Keeping the two watermarks apart bites on an ordinary shutdown, no power loss required: the
/// daemon's shutdown path runs `link_task.abort()` (the transport is gone) **before**
/// `shutdown()` → exit settlement. Exit settlement then commits and pushes successfully with no
/// consumer left to deliver the notification; `wait_for_connection_delivery_within` returns
/// Pending/Stale and `pending_settlement` holds the sha in memory — then the process exits and
/// memory is gone. The session that comes back has that field `None`, and asking git also
/// answers `None` because HEAD is already remotely reachable: the settlement event is lost for
/// good, the local repo plainly holds that turn, and the session is missing a stretch on the hub.
///
/// So the notification side keeps its own receipt: written **before the push**, deleted only on
/// `DeliveryStatus::Delivered`. Better to resend than not to send — the hub converges by sha, a
/// duplicate `commit.settled` pointing at the same commit is idempotent, and nobody can recover
/// the one that was dropped.
///
/// It lives under `<repo>/.git/`: settlement already creates its temporary result file there
/// (`.git` is always a directory and shares this repo's lifetime), so deleting the repo takes
/// the receipts with it instead of leaving orphans in `$AGIT_HOME` pointing at commits that do
/// not exist.
///
/// The file name is a digest of the branch name rather than the name itself: a branch name may
/// carry `/`, and characters that are illegal on Windows (`git check-ref-format` accepts them,
/// NTFS does not). Several sessions run concurrently under one repo directory; one receipt per
/// branch, none overwriting another.
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
async fn guarded_output(
    state: &mut tokio::sync::watch::Receiver<SettlementState>,
    lease: SettlementState,
    mut command: tokio::process::Command,
) -> Option<std::process::Output> {
    if !settlement_lease_is_current(state, lease) {
        return None;
    }
    command.kill_on_drop(true);
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
    let child = command.spawn().ok()?;

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
    let mut group = ProcessGroupGuard(child.id().map(|id| id as i32));
    let mut wait = Box::pin(child.wait_with_output());
    loop {
        tokio::select! {
            biased;
            changed = state.changed() => {
                if changed.is_err() || !settlement_lease_is_current(state, lease) {
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

fn confirmed_strict_push(push: &std::process::Output, sha: &str) -> Option<String> {
    push.status.success().then(|| sha.to_string())
}

/// A local commit the hub has not confirmed taking — **derived** from git's own bookkeeping
/// rather than recalled from memory.
///
/// `pending_settlement` lives only in `Session`: after a failed push (an unreachable network is
/// the most common cause) the daemon crashes, is SIGKILLed or loses power, and the session
/// `resume_from` brings back has that field `None`; if no new turn follows,
/// `strict_settlement_candidate` returns straight out of `None => Ok(None)` — nobody ever pushes
/// that commit and `commit.settled` never names it: the local repo plainly holds that turn, and
/// the session is missing a stretch on the hub.
///
/// So the test reads the remote-tracking ref `git push` maintains itself: it advances only on a
/// successful push. Derived rather than persisted, because any write-on-exit scheme misses a
/// hard kill and a power loss — exactly the occasions that leave a pending commit behind.
///
/// What it asks is **reachability** (`HEAD --not --remotes=origin`), not whether this equals the
/// tip of `origin/<branch>`. The difference bites in a real case: a session that just finished
/// landing ends before running a single turn, so the branch exists but was never pushed and HEAD
/// still sits on the main baseline that came down with the clone. Comparing tips calls that
/// baseline commit pending, so the supervisor creates the branch, pushes it, and sends a
/// `commit.settled` naming a commit this session never produced — a turn on the hub out of
/// nowhere. Asked by reachability, `origin/main` containing it means "already taken" and nothing
/// happens.
///
/// Excluding the remote refs is not enough; the local main file line has to be excluded too:
/// when the agent repo on the hub is empty (a new project binding for the first time), `rc land`
/// clones back a repo with no commits at all and then builds the main baseline locally through
/// `create_main_file_line`, with not one `origin/*` ref. A session that ends without running a
/// turn leaves HEAD on that purely local main baseline — asked by origin reachability alone it
/// counts as never pushed, so a `commit.settled` naming a scaffold commit goes out anyway, which
/// is the "turn out of nowhere" the paragraph above avoids. The main line is not this session's
/// output and settlement must never report it.
///
/// `--glob=refs/heads/main*` rather than `^refs/heads/main`: with no main, the latter makes the
/// whole rev-list fatal (the derivation degenerates into always `None`), while a `--glob` that
/// matches nothing counts as unwritten. Widening to `main-...` only makes the test more
/// conservative — it can skip a push, never invent one.
async fn unpushed_local_head(
    state: &mut tokio::sync::watch::Receiver<SettlementState>,
    lease: SettlementState,
    repo_dir: &str,
    head: &str,
) -> Option<String> {
    let remotes = format!("--remotes={}", crate::domain::repo::ORIGIN);
    let mut rev = tokio::process::Command::new("git");
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

/// `kill_on_drop` stops the direct child. Settlement commands can be waiting on
/// git grandchildren, so on Unix the lease also owns the process group. Windows
/// uses a suspended-at-birth kill-on-close Job Object in [`guarded_output`].
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
            // expected when the child won the race and already exited.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

/// How often to poll the transcript file. The harness appends every few hundred
/// ms at most, so a poll this short is imperceptible and, unlike inotify,
/// behaves the same on every platform and when the file is *replaced* (which
/// `agit resume`'s slow path does).
pub const TAIL_POLL_MS: u64 = 120;

/// How long, in milliseconds, to wait at most for the transcript file to go quiet before
/// declaring a turn over.
pub const SETTLE_MAX_MS: u64 = 1500;

/// The raw material for deciding a pending approval again. See `Session::pending`.
struct PendingApproval {
    tool: String,
    /// Unredacted. It stays in this machine's memory and never reaches the wire.
    input: serde_json::Value,
    /// A session-scoped Claude approval changes the global driver mode. Keep
    /// the machine-originated prediction beside the authorization evidence so
    /// the execution side can require a durable danger permit before echoing a
    /// bypass suggestion.
    suggested_permission_mode: Option<PermissionMode>,
}

#[derive(Debug)]
struct InitialTurn {
    message: String,
    by: Option<String>,
}

struct PendingTurnCommand {
    message: String,
    by: Option<String>,
    /// `None` is the creation-time fire-and-forget prompt. Viewer-originated
    /// turns always retain their ticket until the exact native outcome.
    reply: Option<Ticket<TurnStartOutcome>>,
    initial: bool,
    guard_attempt: Option<crate::rc::harness::TurnGuardAttempt>,
}

enum PendingInitialReply {
    Attached,
    Blocked(Ticket<TurnStartOutcome>),
    Absent(Ticket<TurnStartOutcome>),
}

#[derive(Clone)]
struct ResolvedInitialTurn {
    message: String,
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
    Ready,
    Observe { confirmation_token: String },
    Confirm { confirmation_token: String },
    FailClosed { confirmation_token: String },
}

/// A live session under supervision.
pub struct Session {
    pub info: SessionInfo,
    driver: AnyDriver,
    tailer: Option<Tailer>,
    redactor: redact::Redactor,
    /// Emitted upward; the daemon numbers these and ships them to the hub.
    out: mpsc::Sender<Frame>,
    /// An approval already sent and not yet answered → **the raw material for deciding whether
    /// it needs the owner** (the tool name plus the unredacted input).
    ///
    /// The material is stored rather than the verdict, because a verdict **expires**: minutes
    /// can pass between sending an approval and someone answering it, and in between the owner
    /// may `agit rc grant` a grant away or unbind a directory — the allowlist is re-read on
    /// every heartbeat (`reload_grants`). Caching the conclusion "an operator may allow this"
    /// keeps a revoked grant in force on a pending approval, and the direction it errs in is
    /// the permissive one. Deciding again at the moment of the answer, against the list as it
    /// stands **then**, makes a tightening take effect immediately, and a loosening need not
    /// wait for the next approval.
    ///
    /// The input is stored unredacted: the classifier compares real paths against the allowlist
    /// (redaction replaces the home directory with a pseudonym, so the comparison is bound to
    /// miss). It stays in this machine's memory and never reaches the wire — the copy sent to
    /// viewers is scrubbed before emit.
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
    /// Tells the daemon the harness's own id. A separate channel rather than the event stream:
    /// this id never reaches the wire (viewers address only the logical id), but the daemon
    /// needs it for the double-writer guard and the roster.
    notes: mpsc::Sender<SessionNote>,
    /// The project directory. `agit commit` / `agit push` run here.
    cwd: PathBuf,
    /// Which generation of the same logical id this session is. Reported back with `Ended` on
    /// exit so the daemon removes only its own generation.
    generation: u64,
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
    /// A local commit that exists but has not received a successful push
    /// result yet. A later turn retries the idempotent push even when strict
    /// commit reports no new transcript content.
    pending_settlement: Option<PendingSettlement>,
    /// See [`SettlementChild`]: test-only, always `None` in production.
    settlement_child: Option<SettlementChild>,
    /// Creation may supply a prompt before Codex has a thread id. It is the
    /// only prompt allowed to wait for Ready without an RPC receipt.
    queued_initial_turn: Option<InitialTurn>,
    /// Metadata and optional viewer receipt for the one native `turn/start`
    /// currently awaiting its exact response.
    pending_turn_command: Option<PendingTurnCommand>,
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
    /// A delta is a transport chunk, not a semantic boundary; every item keeps its own safety
    /// tail.
    delta_streams: std::collections::HashMap<String, redact::StreamRedactor>,
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
        by: Option<String>,
    },
    Turn {
        message: String,
        by: Option<String>,
        guard_attempt: Option<crate::rc::harness::TurnGuardAttempt>,
        reply: Ticket<TurnStartOutcome>,
    },
    Steer {
        message: String,
        by: Option<String>,
        reply: Ticket<Delivery>,
    },
    Interrupt {
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
    Shutdown,
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
        Command::InitialTurn { .. } => true,
        Command::Turn { reply, .. } => reply.accept(),
        Command::Steer { reply, .. } => reply.accept(),
        Command::Interrupt { reply } => reply.accept(),
        Command::Approve { reply, .. } => reply.accept(),
        Command::SetPermissionMode { reply, .. } => reply.accept(),
        Command::Shutdown => true,
    }
}

fn command_danger_arm(command: &Command) -> Option<u64> {
    match command {
        Command::Approve { danger, .. } => danger.arm(),
        Command::SetPermissionMode { armed, .. } => *armed,
        _ => None,
    }
}

/// Trim an approval answer given by a **non-owner** down to what it may be.
///
/// The whole `ApprovalResponse` comes from the browser, and two of its fields reach well past
/// answering this one request:
///
/// * **`scope`**: `Session` hands the request's own `permission_suggestions` back to
///   claude-code unchanged, and `suggested_mode` maps `bypassPermissions` to
///   `PermissionMode::Bypass`. A call an operator may answer, deliberately judged "genuinely
///   confined" (`Write src/main.rs`), becomes a side door to a session-level mode, while the
///   front door (`session.setPermissionMode`) is locked to the owner on both sides. "An
///   operator may answer **this one**" must never be read as "an operator may answer every one,
///   forever".
///
/// * **`message`** (the refusal reason): claude-code fills it verbatim into
///   `{"behavior":"deny","message":...}` back to the CLI, the model reads it into context as
///   the reason for the refusal and then acts on it — a refusal reason and an instruction are
///   the same thing to the model. Refusal takes `Need::Brake`, which deliberately does not test
///   `require_owner_to_drive`, on the grounds that a brake needs no permission. That holds for
///   an interrupt and for tightening a mode, which carry no payload; it does not hold for a
///   refusal carrying free text. An operator explicitly kept out by `turn.start` /
///   `turn.steer` then has a path for planting arbitrary instructions into an owner-only
///   session's context. A fixed sentence replaces it — it still tells the model this was
///   refused, and that is all a refusal has to convey.
///
/// Both fields are the same kind of thing, so they are contained in the same place: the next
/// field that comes across from the browser is added here and cannot be missed.
fn sanitize_for_non_owner(response: &mut ApprovalResponse) {
    response.scope = crate::protocol::ApprovalScope::Once;
    if matches!(response.decision, crate::protocol::ApprovalDecision::Deny) {
        response.message = Some("denied by a reviewer in the shared workspace".into());
    }
}

fn enforce_approval_caller_fields(response: &mut ApprovalResponse, caller_is_owner: bool) {
    if !caller_is_owner {
        sanitize_for_non_owner(response);
    }
}

fn approval_answer_is_blocked(
    needs_owner: bool,
    caller_is_owner: bool,
    decision: crate::protocol::ApprovalDecision,
) -> bool {
    let refusing = matches!(decision, crate::protocol::ApprovalDecision::Deny);
    needs_owner && !caller_is_owner && !refusing
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
    /// Drop both halves of every approval the harness can no longer answer.
    ///
    /// The supervisor copy is the machine-side authorization evidence; the
    /// driver copy is the native request id/suggestion needed to write the
    /// answer. They form one lifecycle even though their payloads differ, so a
    /// turn boundary must clear both in the same production function.
    fn abandon_pending_approvals(&mut self) -> bool {
        let supervisor = self.pending.len();
        self.pending.clear();
        let driver = self.driver.abandon_pending_approvals();
        supervisor != 0 || driver != 0
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
        info: SessionInfo,
        spec: LaunchSpec,
        out: mpsc::Sender<Frame>,
        notes: mpsc::Sender<SessionNote>,
        confinement: tokio::sync::watch::Receiver<crate::rc::Confinement>,
        settlement: tokio::sync::watch::Receiver<SettlementState>,
        generation: u64,
        secret_filter: crate::domain::secret_filter::MatcherHandle,
    ) -> Result<Session, crate::rc::harness::proc::LaunchError> {
        let agit_session = spec.agit_session.clone();
        let cwd = spec.cwd.clone();
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
        let driver = AnyDriver::launch(&info.runtime, spec).await?;
        let mut s = Session {
            agit_session,
            cwd,
            info,
            driver,
            tailer: None,
            redactor: redact::Redactor::with_registered(
                redact::Persona::this_machine(),
                secret_filter,
            ),
            out,
            pending: Default::default(),
            consumed_bytes: 0,
            landed_thread: None,
            notes,
            confinement,
            settlement,
            pending_settlement: None,
            settlement_child: None,
            queued_initial_turn: None,
            pending_turn_command: None,
            resolved_initial_turn: None,
            announced_turn_ids: Default::default(),
            delta_streams: Default::default(),
            alerted_registered: Default::default(),
            generation,
            resuming,
        };
        // claude-code's id is one we chose ourselves and is known at this moment — so
        // **announce** it right away rather than waiting for `Ready` (which arrives only after
        // the first user message). codex takes the Ready path.
        //
        // Announce only, **do not land**: `land()` spawns a subprocess for an HTTP `get_agent`
        // and a `git clone`, while `Session::launch` is awaited inside `Daemon::dispatch` — all
        // of which holds the daemon's global lock, inside the main select loop. One clone can
        // freeze every RPC on this machine, and the **event pump** with them, for tens of
        // seconds.
        //
        // Landing happens at the top of `run()`, out of the lock and in its own task. It is
        // retryable anyway (another attempt before every settlement), so arriving later costs
        // nothing else.
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
        })
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

    /// The same announcement, but it **never waits** for room in the channel. For
    /// `Session::launch` only.
    ///
    /// `notes` is a bounded channel whose only consumer is the `notes_rx.recv()` branch of the
    /// daemon's main select loop — while `launch` runs, that loop is awaiting this very
    /// `launch` under the global lock and takes no further frame. So once the channel fills,
    /// `send().await` waits on a queue **nobody will ever drain**: the daemon deadlocks
    /// together with the event pump, and it cannot be broken — draining the queue needs the
    /// lock released, and releasing the lock needs this send to return.
    ///
    /// Dropping this one does no harm, so `try_send` is enough here: `Bound` is idempotent,
    /// `bind_if_known()` at the top of `run_inner()` sends it again as soon as the lock is out,
    /// and every settlement sends it again after that.
    async fn announce_binding_without_waiting(&mut self) {
        let Some(note) = self.binding_note() else {
            return;
        };
        let _ = self.notes.try_send(note);
    }

    /// Announce, then land the local lineage. Idempotent, callable repeatedly.
    ///
    /// **Do not call this from `Session::launch`** — see the comment there: landing spawns
    /// subprocesses for the network and a git clone, while launch is awaited under the daemon's
    /// global lock inside the main select loop.
    async fn bind_if_known(&mut self) {
        self.announce_binding().await;
        let Some(thread_id) = self.driver.runtime_thread_id() else {
            return;
        };
        let Some(lease) = settlement_lease(&self.settlement) else {
            return;
        };
        self.land(&thread_id, lease).await;
    }

    /// The executable for settlement and landing subprocesses. See [`SettlementChild`].
    fn settlement_exe(&self) -> Option<PathBuf> {
        match &self.settlement_child {
            Some(child) => Some(child.exe.clone()),
            None => std::env::current_exe().ok(),
        }
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

    /// Build where this session lands on this machine: the repo, the main file line, the
    /// branch, the store link.
    ///
    /// Without it, the `agit commit --from-hook` the supervisor runs fails silently on "no such
    /// repo / the branch was never born / no link" — the conversation on the web never settles
    /// into a commit. So failure is not the end: `landed` stays false and the next settlement
    /// tries again.
    async fn land(&mut self, thread_id: &str, lease: SettlementState) {
        if !settlement_lease_is_current(&self.settlement, lease) {
            return;
        }
        // A previous success is not permanent authority: the slug can be
        // deleted and reused while the session stays alive. Clear the cached
        // proof before every network revalidation so failure cannot fall
        // through to commit/push.
        self.landed_thread = None;
        let Some(agit_session) = self.agit_session.clone() else {
            // Unmanaged (no project bound) means there is nowhere to land, which is not a
            // failure.
            self.landed_thread = Some(thread_id.to_string());
            return;
        };
        // `commands::rc::land_argv` builds the argv — it lives next to the clap definition, so
        // renaming a flag has exactly one site to change, and a test really parses this argv.
        // Built by hand here, one rename becomes a subprocess call that always fails and only
        // mutters about it in the log.
        let args = crate::commands::rc::land_argv(
            &agit_session.slug(),
            agit_session.agent_id(),
            agit_session.branch(),
            &self.info.runtime,
            thread_id,
            &self.cwd.to_string_lossy(),
        );
        let out = if let Some(exe) = self.settlement_exe() {
            let mut command = tokio::process::Command::new(exe);
            command.args(&args).env(
                crate::hub::identity::EXPECTED_AGENT_ID_ENV,
                agit_session.agent_id(),
            );
            guarded_output(&mut self.settlement, lease, command).await
        } else {
            None
        };
        match out {
            Some(o) if o.status.success() => self.landed_thread = Some(thread_id.to_string()),
            Some(o) => tracing_note(&format!(
                "lineage landing failed for {agit_session}: {} (will retry next turn)",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            None => tracing_note(&format!(
                "lineage landing could not run for {agit_session} (will retry next turn)"
            )),
        }
    }

    /// The harness-native session/thread id, once the driver knows it.
    /// claude-code knows it at launch (we chose it); codex learns it on Ready.
    pub fn runtime_thread_id(&self) -> Option<String> {
        self.driver.runtime_thread_id()
    }

    /// Broadcast the driver's current mode if it has drifted from what viewers
    /// were last told. Cheap enough to call after anything that might move it.
    async fn announce_mode(&mut self, by: Option<String>) {
        let now = self.driver.permission_mode();
        if self.info.permission_mode == Some(now) {
            return;
        }
        self.info.permission_mode = Some(now);
        self.emit(
            method::SESSION_PERMISSION_MODE,
            SessionPermissionMode {
                session_id: self.info.session_id.clone(),
                mode: now,
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
        self.abandon_pending_approvals();
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
                by: pending.by.clone(),
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
                Some((pending.by.clone(), pending.message.clone())),
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
        let ends_session = matches!(
            &outcome,
            TurnStartOutcome::Unknown { .. } | TurnStartOutcome::FatalNotAccepted { .. }
        );
        match &outcome {
            TurnStartOutcome::Accepted { .. } => {
                self.announce_mode(pending.by.clone()).await;
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
            | TurnStartOutcome::ConcurrentNotAccepted { .. } => {}
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

    async fn begin_turn_start(&mut self, pending: PendingTurnCommand) {
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
        let dispatch = self
            .driver
            .start_turn(
                &pending.message,
                !pending.initial,
                pending.guard_attempt.clone(),
            )
            .await;
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
        guard_attempt: Option<&TurnGuardAttempt>,
        reply: Ticket<TurnStartOutcome>,
    ) -> PendingInitialReply {
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
        if pending.message != message || pending.reply.is_some() {
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
                by: initial.by,
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

    /// Take one redaction result: `registered_ids` becomes an alert, the text goes out as is.
    async fn finish_scrub(&mut self, report: redact::Report, source: &str) -> String {
        self.alert_registered(report.registered_ids, source).await;
        report.text
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

    /// End one item's streaming redaction: emit the safety tail it holds back and drop the
    /// stream.
    async fn flush_delta(&mut self, item_id: &str) {
        let Some(mut stream) = self.delta_streams.remove(item_id) else {
            return;
        };
        let report = stream.flush();
        if report.text.is_empty() {
            return;
        }
        let text = self.finish_scrub(report, "item_delta").await;
        self.emit(
            method::ITEM_DELTA,
            ItemDelta {
                item_id: item_id.to_string(),
                text,
            },
        )
        .await;
    }

    /// A held-back tail leaves only through here, so every turn boundary and every exit path
    /// passes through it: missing one means that stretch of text **never** appears on the web.
    async fn flush_all_deltas(&mut self) {
        let ids: Vec<String> = self.delta_streams.keys().cloned().collect();
        for id in ids {
            self.flush_delta(&id).await;
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
        resolved_attribution: Option<(Option<String>, String)>,
        mark_running: bool,
    ) -> Result<Option<PreparedTurnStarted>, String> {
        crate::rc::harness::validate_native_turn_id(&turn_id)
            .map_err(|error| format!("supervisor turn.started: {error}"))?;
        if !self.remember_announced_turn(turn_id.clone())? {
            return Ok(None);
        }
        let (by, command_prompt) = if let Some((by, prompt)) = resolved_attribution {
            (by, Some(prompt))
        } else if let Some(pending) = self.pending_turn_command.as_ref() {
            (pending.by.clone(), Some(pending.message.clone()))
        } else {
            (None, None)
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
                source: if by.is_some() {
                    TurnSource::Remote
                } else {
                    TurnSource::Local
                },
                by,
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
        resolved_attribution: Option<(Option<String>, String)>,
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
        self.info.status = status;
        self.info.updated_at = chrono::Utc::now().to_rfc3339();
        self.emit(
            method::SESSION_STATUS,
            serde_json::json!({"session_id": self.info.session_id, "status": status}),
        )
        .await;
    }

    /// Run until the harness exits or we are told to stop.
    pub async fn run(mut self, mut commands: mpsc::Receiver<Command>) {
        let id = self.info.session_id.clone();
        let generation = self.generation;
        let notes = self.notes.clone();
        self.run_inner(&mut commands).await;
        // Final safety net for EOF/command-channel shutdown paths. The maps are
        // about native requests owned by this process; none survive its exit.
        self.abandon_pending_approvals();
        // **The reason for exiting does not decide whether the process tree is reaped.**
        //
        // An exit path that returns directly — the harness's own EOF/Exited, a closed command
        // channel — leaves the compilers, servers and background shells it started on the
        // machine. Funnelling into the single finally point means a newly added exit branch
        // cannot miss it either. A failed cleanup leaves a diagnostic behind but must not
        // swallow Ended — the daemon still has to remove a finished session from its table.
        if let Err(e) = self.driver.shutdown().await {
            tracing_note(&format!("[{id}] harness cleanup failed: {e}"));
        }
        // **A session ending is a settlement boundary too.** Funnelled into the same single
        // finally point: Shutdown, driver EOF and harness Exited each return on their own, and
        // adding the call to them one by one always misses one.
        self.settle_on_exit().await;
        // **Every exit path announces.** Here rather than one line before each `return`,
        // because forgetting to announce on a newly added exit path has no symptom at all —
        // that row stays silently in `sessions` while its channel is dead.
        let _ = notes
            .send(SessionNote::Ended {
                session_id: id,
                generation,
            })
            .await;
    }

    async fn run_inner(&mut self, commands: &mut mpsc::Receiver<Command>) {
        // Out of the daemon's lock and in its own task, the slow work (HTTP + git clone) can
        // happen now.
        self.bind_if_known().await;
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(TAIL_POLL_MS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                cmd = commands.recv() => {
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
                    match cmd {
                        None | Some(Command::Shutdown) => {
                            self.abandon_pending_approvals();
                            self.set_status(SessionStatus::Ended).await;
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
                        Some(Command::InitialTurn { message, by }) => {
                            if self.driver.runtime_thread_id().is_none() {
                                // This is the one fire-and-forget path allowed
                                // to wait for Codex Ready. A viewer TURN_START
                                // always receives a retryable not-accepted
                                // result instead of a false success receipt.
                                if self.queued_initial_turn.is_none() {
                                    self.queued_initial_turn = Some(InitialTurn { message, by });
                                } else {
                                    tracing_note(&format!(
                                        "[{}] ignored a duplicate creation prompt while Codex was opening",
                                        self.info.session_id
                                    ));
                                }
                            } else {
                                self.begin_turn_start(PendingTurnCommand {
                                    message,
                                    by,
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
                            by,
                            guard_attempt,
                            reply,
                        }) => {
                            let reply = match self.coalesce_pending_initial_reply(
                                &message,
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
                                if initial.message != message {
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
                                    by: initial.by,
                                    reply: Some(reply),
                                    initial: true,
                                    guard_attempt,
                                })
                                .await;
                            } else {
                                self.begin_turn_start(PendingTurnCommand {
                                    message,
                                    by,
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
                        Some(Command::Steer { message, by: _, reply }) => {
                            // The steer reaches the local harness unchanged — redaction is
                            // only for the copy that leaves the machine. The hit is still
                            // reported: a registered secret that enters this session through a
                            // steer is redacted **silently** when the harness repeats it.
                            let report = self.redactor.scrub(&message);
                            self.alert_registered(report.registered_ids, "turn_steer").await;
                            let result = self.driver.steer(&message).await;
                            if let Some(message) = result
                                .as_ref()
                                .err()
                                .filter(|error| {
                                    crate::rc::harness::is_request_id_exhaustion(error)
                                })
                                .map(ToString::to_string)
                            {
                                self.handle_protocol_invariant(message, None, None).await;
                                reply.finish(result);
                                return;
                            }
                            reply.finish(result);
                        }
                        Some(Command::Interrupt { reply }) => {
                            let r = self.driver.interrupt().await;
                            if let Some(message) = r
                                .as_ref()
                                .err()
                                .filter(|error| {
                                    crate::rc::harness::is_request_id_exhaustion(error)
                                })
                                .map(ToString::to_string)
                            {
                                self.handle_protocol_invariant(message, None, None).await;
                                reply.finish(r);
                                return;
                            }
                            let abandoned = r.is_ok() && self.abandon_pending_approvals();
                            if let Some(status) =
                                status_after_approval_interrupt(self.info.status, abandoned)
                            {
                                // The approval card is dead as soon as the
                                // interrupt is accepted. Do not leave the
                                // composer gated until a later completion echo.
                                self.set_status(status).await;
                            }
                            reply.finish(r);
                        }
                        Some(Command::Approve {
                            mut response,
                            caller_is_owner,
                            danger,
                            reply,
                        }) => {
                            // Decide again on the machine side whether this one needs the
                            // owner.
                            //
                            // This is the classifier's only execution point. Every frame that
                            // reaches here today has passed the hub's gate, so this check stops
                            // no **current** attack — what it stops is a hub rolled back to a
                            // build without that gate, a second pod, a fallback query that
                            // finds nothing: the class agitd is meant to withstand on its own
                            // under the trust model. An unrecognized approval_id is always
                            // refused: forwarding an approval we never sent lets whoever
                            // guesses an id answer in someone else's place.
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
                            // **Decide at the moment of the answer, not with the verdict from
                            // the moment it was sent.**
                            //
                            // The allowlist can change between the two (`agit rc grant`
                            // revoking one, a directory unbound), and a cached "an operator may
                            // allow this" keeps a revoked grant in force on a pending approval
                            // — erring on the permissive side. The list is re-read on every
                            // heartbeat and deciding costs a few path comparisons, which is not
                            // worth a cache that errs outward.
                            let conf = self.confinement.borrow().clone();
                            let needs_owner = crate::rc::policy::approval_owner_reason(
                                &pending.tool,
                                &pending.input,
                                &conf.roots,
                                &self.cwd,
                                &conf.operator_heads,
                            )
                            .is_some();
                            // **This gate stops only "allow".**
                            //
                            // A refusal is a brake, and a brake needs no permission — the
                            // daemon decides it exactly that way (Deny takes `Need::Brake`, see
                            // the docs on `Need`). Stopping a refusal here too makes the two
                            // sides say different things: a high-risk approval becomes
                            // **refusable only by the owner**, the owner may be asleep, and the
                            // session hangs on `awaiting_approval`. Anyone being able to hit the
                            // brake is what this screen has to be.
                            //
                            // The free text a refusal carries is another matter and is replaced
                            // below (`sanitize_for_non_owner`) — that is what a refusal really
                            // has to be guarded against, not the refusal itself.
                            if approval_answer_is_blocked(
                                needs_owner,
                                caller_is_owner,
                                response.decision,
                            ) {
                                // Put it back: it is still waiting for an answer.
                                self.pending.insert(response.approval_id.clone(), pending);
                                reply.finish(Ok(ApprovalOutcome::ExplicitRefusal {
                                    message: "this approval reaches past what an operator may green-light, so only the owner may allow it".into(),
                                    retained: true,
                                }));
                                continue;
                            }
                            // **The answer's scope is decided too.**
                            //
                            // `scope` comes from the browser like every other field, and
                            // neither side looks at it: `scope == Session` hands the request's
                            // own `permission_suggestions` back to claude-code unchanged, and
                            // `suggested_mode` maps `bypassPermissions` to
                            // `PermissionMode::Bypass`. A call an operator may answer,
                            // deliberately judged "genuinely confined" — `Write src/main.rs` —
                            // is a side door to a session-level mode, while the front door
                            // (`session.setPermissionMode`) is locked to the owner on both
                            // sides.
                            //
                            // "An operator may answer **this one**" must never be read as "an
                            // operator may answer every one, forever".
                            enforce_approval_caller_fields(&mut response, caller_is_owner);
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
                                ApprovalOutcome::Applied { .. } => {
                                    if self.pending.is_empty() {
                                        self.set_status(SessionStatus::Running).await;
                                    }
                                    // Answering "allow, and stop asking" moves
                                    // the mode underneath us; tell every viewer.
                                    self.announce_mode(None).await;
                                }
                                ApprovalOutcome::ExplicitRefusal { .. } => {}
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
                        Some(Command::SetPermissionMode {
                            mode,
                            by,
                            armed,
                            reply,
                        }) => {
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
                                    self.info.permission_mode =
                                        Some(self.driver.permission_mode());
                                    self.emit(
                                        method::SESSION_PERMISSION_MODE,
                                        SessionPermissionMode {
                                            session_id: self.info.session_id.clone(),
                                            mode,
                                            applied,
                                            by,
                                        },
                                    )
                                    .await;
                                    PermissionModeOutcome::Applied { applied }
                                }
                                Err(error) if error.is_explicit_refusal() => {
                                    PermissionModeOutcome::ExplicitRefusal {
                                        message: error.to_string(),
                                    }
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
                            self.abandon_pending_approvals();
                            self.set_status(SessionStatus::Ended).await;
                            return;
                        }
                        Some(HarnessEvent::Exited { code }) => {
                            self.abandon_pending_approvals();
                            // Drain whatever the harness wrote on its way out.
                            // The held-back delta tails go first, ahead of the
                            // transcript's authoritative items for the same text.
                            self.flush_all_deltas().await;
                            self.drain_transcript().await;
                            self.emit(method::SESSION_STATUS, serde_json::json!({
                                "session_id": self.info.session_id,
                                "status": SessionStatus::Ended,
                                "exit_code": code,
                            })).await;
                            return;
                        }
                        Some(e) => {
                            self.on_harness_event(e).await;
                            if self.info.status == SessionStatus::Ended {
                                return;
                            }
                        }
                    }
                }

                _ = ticker.tick() => {
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
                    self.drain_transcript().await;
                }
            }
        }
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

    async fn on_harness_event(&mut self, ev: HarnessEvent) {
        match ev {
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
                        // Start at the end when resuming an existing transcript:
                        // the history is already in the repo, only new lines are
                        // news. A fresh session must read from the start — see `resuming`.
                        self.tailer = Some(Tailer::new(p, !self.resuming));
                    }
                }
                // Codex has a pre-input native Ready boundary. Claude does not:
                // its first Ready arrives only after the first user message,
                // which would deadlock a no-prompt recovery behind the daemon's
                // Drive gate. Claude therefore uses the internal command-loop
                // barrier above; Codex continues to require native evidence.
                if self.info.runtime == "codex" {
                    self.sync_turn_guard(TurnGuardBarrier::Ready).await;
                }
                self.flush_initial_turn_if_ready().await;
            }
            HarnessEvent::TurnStartResolved(outcome) => {
                let Some(pending) = self.pending_turn_command.take() else {
                    tracing_note(&format!(
                        "[{}] ignored a turn/start response with no pending command",
                        self.info.session_id
                    ));
                    return;
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
                self.delta_streams
                    .insert(item_id.clone(), self.redactor.stream());
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
                // **One delta is not a whole string.** The harness can cut a registered secret
                // between two chunks; redacted chunk by chunk neither half matches, and the
                // complete value leaves the machine across two frames. So every item has its
                // own streaming redactor, holding back a tail that could still grow into a rule
                // match; `flush_delta` releases what is held when the item ends.
                if !self.delta_streams.contains_key(&item_id) {
                    let stream = self.redactor.stream();
                    self.delta_streams.insert(item_id.clone(), stream);
                }
                let report = self
                    .delta_streams
                    .get_mut(&item_id)
                    .expect("delta stream was inserted above")
                    .push(&text);
                if !report.text.is_empty() {
                    let text = self.finish_scrub(report, "item_delta").await;
                    self.emit(method::ITEM_DELTA, ItemDelta { item_id, text })
                        .await;
                }
            }
            HarnessEvent::ItemCompleted { item_id } => {
                self.flush_delta(&item_id).await;
                // Intentionally ignored: the authoritative `item.completed`
                // comes from the transcript file, with its object hash. Emitting
                // one here too would put two different records of the same item
                // on the wire.
            }
            HarnessEvent::TurnCompleted {
                turn_id,
                outcome,
                cost_usd,
                duration_ms,
            } => {
                if let Err(message) = self
                    .announce_turn_started_once(turn_id.clone(), None, None, false)
                    .await
                {
                    self.handle_protocol_invariant(message, None, None).await;
                    return;
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
                self.abandon_pending_approvals();
                // Release the held-back delta tails before the turn boundary: `item.completed`
                // is about to come out of the transcript, and the streaming form of the same
                // text has to go ahead of it.
                self.flush_all_deltas().await;
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
                self.set_status(SessionStatus::Idle).await;
                // One turn ended = one settlement = one commit, pushed immediately.
                //
                // This is where "sync on every update" actually lands: the harness's own Stop
                // hook triggers the settlement (`agit commit --from-hook`), and once it has
                // persisted we push the branch to this project's private repo. The push sits on
                // the turn boundary rather than on every event, because a commit is what makes
                // a meaningful version; pushing per event produces a string of half-finished
                // snapshots nobody wants to read.
                self.settle_and_push(SettlementBoundary::Turn).await;

                self.emit(
                    method::TURN_COMPLETED,
                    TurnCompleted {
                        turn_id,
                        outcome: match outcome {
                            TurnOutcome::Ok => PTurnOutcome::Ok,
                            TurnOutcome::Interrupted => PTurnOutcome::Interrupted,
                            TurnOutcome::Error => PTurnOutcome::Error,
                        },
                        error: None,
                        cost_usd,
                        duration_ms,
                    },
                )
                .await;
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
            HarnessEvent::Approval(mut req) => {
                if let Err(message) = self
                    .announce_turn_started_once(req.turn_id.clone(), None, None, true)
                    .await
                {
                    self.handle_protocol_invariant(message, None, None).await;
                    return;
                }
                req.session_id = self.info.session_id.clone();
                // Whether this approval goes back to the owner is decided **here** — only the
                // machine sees the filesystem, and only it can canonicalize a path and compare
                // it against the allowlist. The hub receives the conclusion, not the clues:
                // inferring from `kind` turns a Bash command that `curl`s outward into an
                // `Exec` an operator may allow.
                //
                // Decide with the **unredacted** input, and decide before redaction: redaction
                // replaces the home directory with a pseudonym, and comparing against the
                // allowlist after that is bound to miss.
                // The allowlist and the grant list are read **now**, not copied at launch.
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
                );
                req.requires_owner = reason.is_some();
                req.owner_reason = reason;
                // The verdict **stays on the machine**; sending it out is not enough.
                //
                // A verdict thrown away once computed leaves `pending` holding only the id,
                // while the `approval.decide` path checks ownership alone and never compares
                // role against verdict. The hub would then be the only executor of this whole
                // classification — and the trust model says the hub is a relay, not an
                // authority: every other dangerous verb (changing mode, starting a session with
                // a mode) has a second gate on the machine side.
                //
                // What is stored is the **raw material** (the tool name plus the unredacted
                // input), not the conclusion: the moment of the answer decides again against
                // the allowlist as it stands then, see `Session::pending`. So this input is
                // taken before redaction — the scrubbed copy does not compare against the
                // allowlist.
                self.pending.insert(
                    req.approval_id.clone(),
                    PendingApproval {
                        tool: req.tool.clone(),
                        input: req.input.clone(),
                        suggested_permission_mode: req.suggested_permission_mode,
                    },
                );
                // Scrub the card: a command line is exactly where a token ends up.
                let report = self.redactor.scrub(&req.summary);
                req.summary = self.finish_scrub(report, "approval").await;
                // **`paths` is the third piece of raw material on the same card, not
                // metadata.**
                //
                // What `harness::paths_of` puts here is the **absolute** `file_path` —
                // `/Users/alice/secret/src/main.rs`. With summary and input scrubbed, it still
                // lies unchanged in the same struct and leaves the machine with
                // `approval.request`: everyone in the workspace who sees this card gets the
                // machine owner's username and home directory. The verdict is unaffected: that
                // step is above and uses the unredacted `input`, and `paths` never takes part
                // in it (see `policy::approval_owner_reason`).
                let mut path_ids = vec![];
                req.paths = req
                    .paths
                    .iter()
                    .map(|p| {
                        let report = self.redactor.scrub(p);
                        path_ids.extend(report.registered_ids);
                        report.text
                    })
                    .collect();
                self.alert_registered(path_ids, "approval").await;
                // `input` is structured, so what gets scrubbed is its **decoded strings**.
                // Serializing first and matching wire bytes misses every registered literal
                // containing `"`, `\` or a newline — and an approval card carries exactly the
                // command line about to run, which is where such a value is most likely to
                // appear. Rewriting in place also removes the re-parse step, whose failure
                // branch leaves the **unredacted** input in place.
                //
                // The verdict is unaffected: `self.pending` above already took the
                // **unredacted** input, and the moment of the answer decides against that.
                let report = self.redactor.scrub_json(&req.input);
                req.input = report.value;
                self.alert_registered(report.registered_ids, "approval")
                    .await;
                self.set_status(SessionStatus::AwaitingApproval).await;
                self.emit(method::APPROVAL_REQUEST, req).await;
            }
            HarnessEvent::Notice { text } => {
                tracing_note(&format!("[{}] {text}", self.info.session_id));
            }
            HarnessEvent::ProtocolInvariant {
                message,
                attempted_mode,
                confirmation_token,
            } => {
                self.handle_protocol_invariant(message, attempted_mode, confirmation_token)
                    .await;
            }
            HarnessEvent::Exited { .. } => {}
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
        // Funnelled here for the same reason as the drain: the tail a streaming redactor holds
        // back leaves only on a flush, while Shutdown, driver EOF and harness Exited each
        // return on their own — adding the line to every exit path always misses one, and the
        // symptom is that stretch of text **never** appearing on the web.
        self.flush_all_deltas().await;
        self.drain_transcript().await;
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
        let Some(lease) = settlement_lease(&self.settlement) else {
            return;
        };
        if let Some(delivery) = self
            .pending_settlement
            .as_ref()
            .and_then(|pending| pending.delivery.clone())
        {
            let status = if delivery.epoch() == lease.epoch {
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
                        return;
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
            return;
        };
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
            return;
        }
        let Some(agit_session) = self.agit_session.clone() else {
            return; // this session is unmanaged (no project bound), so there is nowhere to push
        };
        let slug = agit_session.slug();
        let branch = agit_session.branch().to_string();
        let expected_agent_id = agit_session.agent_id().to_string();
        // Lineage computes the repo path itself. Doing `slug.split('/')` here and feeding
        // `repo_dir` is the vine that climbs out of the repo root with `../..`, three files away
        // from the point that validates it.
        let Some(repo_dir) = self.settlement_repo_dir(&agit_session) else {
            return;
        };
        // The durable watermark on the notification side. See [`unacked_settlement_path`]: it
        // asks something **different** from git reachability.
        let receipt_path = unacked_settlement_path(&repo_dir, &branch);

        let Some(exe) = self.settlement_exe() else {
            return;
        };
        let cwd = self.cwd.clone();
        let agit_session_env = agit_session.to_string();
        let command = |args: &[&str]| {
            let mut command = tokio::process::Command::new(&exe);
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
            command
        };

        // The watermark reads **this session's branch** ref, not the main checkout's HEAD: the
        // settlement lands in the branch's own worktree while the main checkout stays on main,
        // so HEAD is identical before and after while the branch has moved on.
        let watermark_ref = settlement_watermark_ref(&branch);
        let read_head = |repo_dir: &str| {
            let mut head = tokio::process::Command::new("git");
            head.args(crate::domain::meta::GIT_SAFE)
                .args([
                    "-C",
                    repo_dir,
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &watermark_ref,
                ])
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0");
            head
        };

        let repo_dir_s = repo_dir.to_string_lossy().into_owned();
        let Some(before) =
            guarded_output(&mut self.settlement, lease, read_head(&repo_dir_s)).await
        else {
            return;
        };
        // Before the branch is born (ahead of the first settlement) there is no prior
        // watermark; the empty string is equal to no commit.
        let before = if before.status.success() {
            String::from_utf8_lossy(&before.stdout).trim().to_string()
        } else {
            String::new()
        };

        // RC Stop hooks deliberately no-op; this cancellable path is the only
        // settlement writer. Losing the negotiated feature kills this process
        // (and its git process group) instead of letting a static child env keep
        // committing after authorization disappeared.
        let Ok(result_file) = tempfile::NamedTempFile::new_in(repo_dir.join(".git")) else {
            return;
        };
        let result_path = result_file.path().to_path_buf();
        let mut strict_commit = command(&["commit", "--from-supervisor"]);
        strict_commit.env(crate::commands::commit::SUPERVISOR_RESULT_ENV, &result_path);
        let Some(commit) = guarded_output(&mut self.settlement, lease, strict_commit).await else {
            return;
        };
        let Some(after) = guarded_output(&mut self.settlement, lease, read_head(&repo_dir_s)).await
        else {
            return;
        };
        if !after.status.success() {
            return;
        }
        let after = String::from_utf8_lossy(&after.stdout).trim().to_string();
        let reported = std::fs::read_to_string(&result_path)
            .ok()
            .map(|sha| sha.trim().to_string())
            .filter(|sha| !sha.is_empty());
        let pending = match self
            .pending_settlement
            .as_ref()
            .map(|pending| pending.sha.clone())
        {
            Some(sha) => Some(sha),
            // With nothing remembered, read the receipt first and then ask git. Neither
            // watermark can be dropped: the receipt covers whether the notification arrived,
            // `unpushed_local_head` covers whether the commit was pushed — after a crash
            // following a successful push only the receipt can speak, and after a crash before
            // the receipt was persisted only git can. Asked only when this turn produced no new
            // commit: on the other branches `pending` is never read, and the subprocess would
            // be started for nothing.
            None if reported.is_none() && after == before => {
                // The receipt counts only while it points at the current HEAD: a HEAD that has
                // moved on means a later settlement followed, whose `commit.settled` covers
                // this one, and accepting a sha that does not match here only makes the
                // predicate say "HEAD moved away" and stops the whole turn.
                match read_unacked_settlement(&receipt_path).filter(|sha| *sha == after) {
                    Some(sha) => Some(sha),
                    None => {
                        unpushed_local_head(&mut self.settlement, lease, &repo_dir_s, &after).await
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
                tracing_note(&format!(
                    "strict RC settlement stopped ({reason}): {}",
                    String::from_utf8_lossy(&commit.stderr).trim()
                ));
                return;
            }
        };
        let Some(sha) = candidate else {
            return; // strict commit succeeded but produced no new turn
        };
        // The receipt is written **before the push**: a crash after a successful push but
        // before the notification arrives is this hole's most common shape (the daemon's
        // shutdown path tears down the transport before running exit settlement, see
        // [`unacked_settlement_path`]). Written after the push it would leave a window between
        // "pushed" and "receipt written", which is exactly the window to close. A receipt left
        // behind by a failed push causes no false positive: it is accepted only while it points
        // at the current HEAD, and that turn is due for a re-push anyway.
        record_unacked_settlement(&receipt_path, &sha, &branch);
        self.pending_settlement = Some(PendingSettlement {
            sha: sha.clone(),
            delivery: None,
            receipt: Some(receipt_path),
        });

        let Some(push) =
            guarded_output(&mut self.settlement, lease, command(&["push", &slug])).await
        else {
            return;
        };
        let Some(confirmed_sha) = confirmed_strict_push(&push, &sha) else {
            tracing_note(&format!(
                "RC push failed; the pending commit will retry next turn: {}",
                String::from_utf8_lossy(&push.stderr).trim()
            ));
            return;
        };
        if !settlement_lease_is_current(&self.settlement, lease) {
            return;
        }
        let delivery = crate::protocol::ConnectionDelivery::new(
            lease.epoch,
            crate::protocol::ConnectionFeature::AgentIdentityV1,
        );
        let mut notification = Frame::notification(
            method::COMMIT_SETTLED,
            CommitSettled {
                session_id: self.info.session_id.clone(),
                agent: Some(slug),
                expected_agent_id: Some(expected_agent_id),
                branch: Some(branch),
                commit_sha: confirmed_sha,
                // The daemon fills in the real value when it numbers the frame — which stream
                // seq this commit covers is a journal coordinate the supervisor cannot see at
                // all. See the daemon's main pump.
                through_seq: 0,
                turns: None,
            },
        );
        notification.connection_delivery = Some(delivery.clone());
        if let Some(pending) = self.pending_settlement.as_mut() {
            pending.delivery = Some(delivery.clone());
        }
        if self.out.send(notification).await.is_err() {
            delivery.invalidate();
            if let Some(pending) = self.pending_settlement.as_mut() {
                pending.delivery = None;
            }
            return;
        }
        match wait_for_connection_delivery_within(
            &mut self.settlement,
            lease,
            &delivery,
            SETTLEMENT_DELIVERY_WAIT,
        )
        .await
        {
            crate::protocol::DeliveryStatus::Delivered => self.settlement_acked(),
            crate::protocol::DeliveryStatus::Stale => {
                if let Some(pending) = self.pending_settlement.as_mut() {
                    pending.delivery = None;
                }
            }
            crate::protocol::DeliveryStatus::Pending => {}
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

    /// Read new transcript lines and emit one `item.completed` per IR event.
    ///
    /// The parse is per-line and stateless, which works because the IR's
    /// `Event::line` coordinate is a physical line number: a line's events do
    /// not depend on the lines before it.
    async fn drain_transcript(&mut self) {
        let Some(tailer) = self.tailer.as_mut() else {
            return;
        };
        let lines = poll_and_mark(tailer, &mut self.consumed_bytes);
        if lines.is_empty() {
            return;
        }

        // Build every item first, then emit. `Box<dyn Adapter>` is not `Send`,
        // so it must not be alive across an `.await` — keeping the parse in its
        // own scope (inside `items_from_lines`) is what lets this whole session
        // run as a spawned task.
        let (items, registered_ids) = items_from_lines(&self.info.runtime, &self.redactor, &lines);
        self.alert_registered(registered_ids, "item_completed")
            .await;
        for item in items {
            self.emit(method::ITEM_COMPLETED, item).await;
        }
    }
}

/// Transcript lines → `item.completed` payloads, plus the device-local rule ids
/// these lines matched. Shared by the supervised session and the read-only watch
/// path — both must produce the exact same items for the same lines, or the two
/// views of one session drift.
pub fn items_from_lines(
    runtime: &str,
    redactor: &redact::Redactor,
    lines: &[crate::rc::tail::TailedLine],
) -> (Vec<ItemCompleted>, Vec<String>) {
    let Ok(adapter) = crate::adapter::get(runtime) else {
        return (vec![], vec![]);
    };
    let mut out = vec![];
    let mut registered = std::collections::HashSet::new();
    for line in lines {
        if line.text.trim().is_empty() {
            continue;
        }
        // Ask the adapter first: most lines in a transcript (summary, meta) produce no event,
        // and those lines are not worth parsing as JSON a second time.
        let Ok(parsed) = adapter.parse(&line.text) else {
            continue;
        };
        if parsed.events.is_empty() {
            continue;
        }

        let Ok(raw) = serde_json::from_str::<serde_json::Value>(&line.text) else {
            // A half-written or corrupt line. `transcript::wrap_lines`
            // skips these too, so skipping matches what gets committed.
            continue;
        };

        // Scrub the **decoded strings**, not the wire bytes. Matching bytes misses every
        // registered literal containing `"`, `\` or a newline; rewriting in place also removes
        // the "scrubbed but no longer parseable" branch — the structure was never touched, so
        // there is no shape to fall back from.
        let scrubbed = redactor.scrub_json(&raw);
        let registered_projection = !scrubbed.registered_ids.is_empty();
        registered.extend(scrubbed.registered_ids);
        let scrubbed_raw = scrubbed.value;
        // Once a registered low-entropy value was removed, sending the hash of
        // the original JSON would give the hub an offline dictionary oracle.
        // Only that case switches identity: ordinary path/persona redaction must
        // retain the committed envelope's original hash so live reconciliation
        // still works.
        let object_hash = projected_object_hash(&raw, &scrubbed_raw, registered_projection);
        let (raw_out, truncated) = cap_raw(scrubbed_raw);

        for (i, mut event) in parsed.events.into_iter().enumerate() {
            if let Some(t) = event.text.take() {
                let r = redactor.scrub(&t);
                registered.extend(r.registered_ids);
                event.text = Some(r.text);
            }
            // **`paths`, like `text`, is raw material, not metadata.**
            //
            // What the adapter puts here is the **absolute** `file_path` —
            // `/Users/alice/secret/src/main.rs`. With `text` scrubbed it still lies unchanged
            // in the same event, leaves the machine with `item.completed` and lands in the
            // hub's database: everyone in the workspace who sees this transcript gets the
            // machine owner's username and home directory. Scrubbing the line's `raw` does not
            // help — `raw` hangs off the first event of the line only, and consumers read
            // `paths` out of the IR.
            //
            // The approval card carries the same trap; see the comment on `req.paths` in
            // `Session::on_harness_event`: **every** string field in a struct gets scrubbed,
            // and `paths` is not an exception.
            event.paths = event
                .paths
                .iter()
                .map(|p| {
                    let r = redactor.scrub(p);
                    registered.extend(r.registered_ids);
                    r.text
                })
                .collect();
            event.line = Some(line.lineno as usize);
            out.push(ItemCompleted {
                item_id: format!("{}#{i}", line.lineno),
                turn_id: String::new(),
                event,
                line: line.lineno,
                object_hash: object_hash.clone(),
                // Only the first event of a line carries the raw copy —
                // one line can produce several events and shipping the
                // same bytes N times is pure waste.
                raw: if i == 0 {
                    raw_out.clone()
                } else {
                    serde_json::Value::Null
                },
                raw_truncated: truncated,
            });
        }
    }
    (out, registered.into_iter().collect())
}

/// The identity the hub is told this line has.
///
/// Ordinary persona/path redaction keeps the **original** hash: that identity is
/// what lets the hub reconcile the live projection against the pushed
/// transcript. A registered low-entropy value is the one exception — see
/// [`items_from_lines`].
fn projected_object_hash(
    raw: &serde_json::Value,
    scrubbed: &serde_json::Value,
    registered_projection: bool,
) -> String {
    transcript::object_hash(if registered_projection { scrubbed } else { raw })
}

/// Cap an oversized raw line. Returns the value to send and whether it was cut.
fn cap_raw(v: serde_json::Value) -> (serde_json::Value, bool) {
    let s = serde_json::to_string(&v).unwrap_or_default();
    if s.len() <= RAW_LINE_CAP {
        return (v, false);
    }
    (
        serde_json::json!({
            "_truncated": true,
            "_bytes": s.len(),
            "_note": "full line is in the committed transcript; open the session to read it"
        }),
        true,
    )
}

/// The daemon logs to stderr; this crate has no tracing dependency and the CLI's
/// `ui` module is behind the `cli` feature, so keep it plain.
fn tracing_note(msg: &str) {
    eprintln!("agitd: {msg}");
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod settle_tests;

#[cfg(test)]
mod approval_card_tests {
    use super::tests::harness_test_session_with_channels;
    use super::*;

    /// This pins that **every** string field on an approval card is scrubbed, `paths` included.
    ///
    /// `harness::paths_of` puts the absolute `file_path` into `paths`. With summary and input
    /// each scrubbed, the untouched `/Users/alice/secret/src/main.rs` in the same struct still
    /// leaves the machine with `approval.request` — everyone in the workspace who sees this
    /// card gets the machine owner's username and home directory.
    #[tokio::test]
    async fn an_approval_cards_paths_leave_the_machine_scrubbed_like_its_input() {
        let driver = AnyDriver::ClaudeCode(Box::new(
            crate::rc::harness::claude_code::ClaudeCodeDriver::test_driver(),
        ));
        let (mut session, mut out, _notes) =
            harness_test_session_with_channels(driver, "claude-code", SessionStatus::Idle);
        session.redactor = redact::Redactor::new(redact::Persona {
            username: Some("alice".into()),
            home: Some("/Users/alice".into()),
            hostname: None,
        });

        session
            .on_harness_event(HarnessEvent::Approval(crate::protocol::ApprovalRequest {
                approval_id: "approval-paths".into(),
                session_id: String::new(),
                turn_id: "turn-paths".into(),
                kind: crate::protocol::ApprovalKind::FileChange,
                tool: "Edit".into(),
                input: serde_json::json!({ "file_path": "/Users/alice/secret/src/main.rs" }),
                summary: "edit /Users/alice/secret/src/main.rs".into(),
                paths: vec!["/Users/alice/secret/src/main.rs".into()],
                timeout_secs: 30,
                requires_owner: false,
                owner_reason: None,
                can_allow_for_session: false,
                suggested_permission_mode: None,
                requested_at: "now".into(),
            }))
            .await;

        let mut card = None;
        while let Ok(frame) = out.try_recv() {
            if frame.method.as_deref() == Some(method::APPROVAL_REQUEST) {
                card = frame.params;
            }
        }
        let card = card.expect("the approval card must still reach the hub");
        let wire = serde_json::to_string(&card).unwrap();
        // precondition: input and summary really scrub to `~`, which is to say the redactor on
        // this machine knows this home directory — otherwise the assertion below only proves
        // the redactor was misconfigured.
        assert_eq!(card["input"]["file_path"], "~/secret/src/main.rs");
        assert_eq!(card["summary"], "edit ~/secret/src/main.rs");
        assert_eq!(card["paths"][0], "~/secret/src/main.rs");
        assert!(
            !wire.contains("/Users/alice"),
            "the operator's real home left with the approval card: {wire}"
        );
        assert!(
            !wire.contains("alice"),
            "the operator's username left with the approval card: {wire}"
        );
    }
}
