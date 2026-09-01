//! The daemon proper: wires the link, the journal, the sessions and the control
//! socket together, and stays up.
//!
//! # Shape
//!
//! ```text
//!            ┌──────────── Daemon ────────────┐
//!  hub ⇄ Link│  journal (seq + ring)          │
//!            │  mirror  (workspaces/allowlist)│
//!            │  sessions: id → mpsc<Command>  │──▶ Session::run (one task each)
//!            └────────────────────────────────┘         │
//!            control.sock (status / stop)               └─▶ AnyDriver + Tailer
//! ```
//!
//! Every frame a session emits goes through the journal — which stamps it with
//! a gap-free `seq` and buffers it for replay — before it reaches the link. So
//! a disconnection cannot lose events: the session keeps running and numbering,
//! and reconnect replays from the ring buffer.
//!
//! # What the daemon refuses to do
//!
//! The hub relays instructions; it does not authorize them. Before acting on
//! any frame the daemon re-checks, locally:
//!
//! * the target workspace belongs to *this* connection (mirror lookup);
//! * the path is inside that workspace's bound projects ([`super::policy`]);
//! * a `fs.readDirectory` stays under `$HOME`.
//!
//! If the hub were compromised, that is the wall.

use crate::protocol::{
    ErrorCode, FILE_PREVIEW_CAP, Frame, FsReadDirectory, FsReadDirectoryResult, FsReadFile,
    FsReadFileResult, LIKELY_ACTIVE_SECS, LOCAL_GIST_BUDGET, LocalSession, ProjectBind,
    ProjectBindResult, RcRegister, RpcError, SessionInfo, SessionList, SessionListResult,
    SessionResume, SessionResumeResult, SessionStart, SessionStartResult, SessionStatus,
    SessionSubscribe, SessionSubscribeResult, SessionWatch, SessionWatchResult, TerminalClose,
    TerminalExited, TerminalInput, TerminalOpen, TerminalOpenResult, TerminalOutput,
    TerminalResize, TurnInterrupt, TurnInterruptResult, TurnStart, TurnStartResult, TurnSteer,
    TurnSteerResult, VERSION, WorkspaceListResult, method,
};
use crate::rc::harness::{
    ApprovalOutcome, LaunchSpec, PermissionModeOutcome, TurnStartConfirmation, TurnStartOutcome,
};
use crate::rc::roster::{self, Roster};
use crate::rc::supervisor::SettlementState;
use crate::rc::supervisor::{Command, DangerAuthorization, Session, SessionNote};
use crate::rc::terminal::{Terminal, TerminalEvent};
use crate::rc::{control, identity, journal::Journal, link, mirror::Mirror, policy};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

/// The monotonic danger bit's tests plus its credential. **Every path that hands a harness
/// transcript over has to collect a slip from here.**
///
/// Free functions for the tests are not enough: they decide correctly, but nothing enforces
/// "remember to call them" — the looser phrasing that asks "was **my own row** ever dangerous"
/// compiles just as well, while what `session.resume` hands to `--resume` is a transcript that
/// several rows can point at.
mod danger;
mod dispatch;
mod guard;
mod projection;
mod pump;
mod session_rpc;
mod sessions;

/// How many adoptable local sessions to list per project at most.
///
/// It exists for the **fallback path**: with the index available the adapter already returns only
/// this cwd's sessions, but when codex's sqlite index is unavailable it degrades to scanning
/// every rollout on the machine. Truncating keeps the worst case a short list, and after sorting
/// by mtime descending what gets cut is always the oldest.
const PER_PROJECT_LIMIT: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalSessionScan {
    /// User-facing list: spend the bounded transcript budget to fill missing gists.
    Listing,
    /// Internal lookup: metadata is sufficient; never open a transcript for a gist.
    Locate,
}

struct LocatedLocal {
    runtime: String,
    cwd: PathBuf,
    project_id: Option<String>,
    likely_active: bool,
}

/// The one extra fact `Daemon::spawn_session` has to carry out on failure: whether this failure
/// **crossed** the materialization boundary — the line in `Proc::spawn` that actually calls
/// `Command::spawn`.
///
/// The boundary is drawn there and not at `Session::launch`'s return: inside it, **before** the
/// child process is spawned, a missing executable, a permission denial, a missing cwd or a
/// process-tree fence that cannot be built all fail, and each of those proves not one harness
/// started. The fact is carried up from that line by [`crate::rc::harness::proc::LaunchError`],
/// and every layer in between only forwards it.
///
/// A keyed `session.start` uses it to tell apart two failures with opposite outcomes. Crossed: a
/// native process may really be running, that start_id can only stay Pending forever, and a
/// second launch is a disaster. Not crossed: provably not one process started (typically the
/// danger bit failing to persist just before launch — ENOSPC, wrong owner on `~/.agit/rc`), and
/// the reservation must be released; leaving it makes every retry take the early-exit branch and
/// get `pending_start_error` (whose hint still says "do not choose a new start_id"), while that
/// Pending row on disk outlives a daemon restart — no process exists anywhere, yet this start is
/// ruined for good.
struct SpawnFailure {
    error: RpcError,
    reached_launch: bool,
}

impl SpawnFailure {
    fn before_launch(error: RpcError) -> Self {
        Self {
            error,
            reached_launch: false,
        }
    }

    fn after_launch(error: RpcError) -> Self {
        Self {
            error,
            reached_launch: true,
        }
    }
}

impl From<SpawnFailure> for RpcError {
    fn from(failure: SpawnFailure) -> Self {
        failure.error
    }
}

pub struct Options {
    pub hub: String,
    pub token: String,
    pub connection_id: Option<String>,
}

fn set_connection_features(
    settlement: &tokio::sync::watch::Sender<SettlementState>,
    epoch: u64,
    agent_identity_v1: bool,
    session_start_idempotency_v1: bool,
) {
    settlement.send_modify(|state| {
        state.epoch = epoch;
        state.agent_identity_v1 = agent_identity_v1;
        state.session_start_idempotency_v1 = session_start_idempotency_v1;
    });
}

fn accepted_connection_features(result: &crate::protocol::RcRegisterResult) -> (bool, bool) {
    let accepted = |wanted: &str| {
        result
            .accepted_features
            .iter()
            .any(|feature| feature == wanted)
    };
    (
        accepted(crate::protocol::feature::AGENT_IDENTITY_V1),
        accepted(crate::protocol::feature::SESSION_START_IDEMPOTENCY_V1),
    )
}

fn advertised_connection_features() -> Vec<String> {
    vec![
        crate::protocol::feature::AGENT_IDENTITY_V1.to_string(),
        crate::protocol::feature::SESSION_START_IDEMPOTENCY_V1.to_string(),
    ]
}

fn connection_epoch_is_current(
    settlement: &tokio::sync::watch::Sender<SettlementState>,
    epoch: u64,
) -> bool {
    settlement.borrow().epoch == epoch
}

/// Enter shutdown: take no new work, and start the RPC hard deadline.
///
/// **Settlement authorization is not revoked here.** Revoking it here — `epoch + 1` with both
/// features cleared — makes every `agit rc stop` exit settlement spin: the first line of
/// `Session::settle_on_exit` takes the lease, so the trailing turn's commit stays in local git
/// while the hub looks as if this session never persisted anything.
///
/// That revocation guards **something else**: after the socket drops, a settlement still running
/// must not go on using the old connection's identity to land/push (the line after `run_once`
/// returns in the `link` loop does exactly that, and a dropped link is its trigger). When we stop
/// on purpose the connection is still up and the identity is unchanged, so that threat is not
/// present. The authorization is revoked instead by [`Daemon::shutdown`], once the fleet's exit
/// settlements have finished (or the grace period expires) — the moment at which "this machine no
/// longer has the right to settle" actually becomes true.
fn begin_daemon_stop(
    stopping: &mut bool,
    link_stopping: &std::sync::atomic::AtomicBool,
    rpc_stop: &tokio::sync::watch::Sender<bool>,
    mut deadline: std::pin::Pin<&mut tokio::time::Sleep>,
) {
    if *stopping {
        return;
    }
    *stopping = true;
    link_stopping.store(true, std::sync::atomic::Ordering::Release);
    rpc_stop.send_replace(true);
    deadline
        .as_mut()
        .reset(tokio::time::Instant::now() + SESSION_RPC_SHUTDOWN_GRACE);
}

/// Resolve the JoinSet at the hard shutdown boundary without losing tail
/// projections from workers that completed in the same scheduler turn.
///
/// Ready completions are removed first. Any workers still live after that are
/// fail-closed while their per-session gates are held, then aborted and joined.
/// Only after this returns is `JoinSet::is_empty()` an authoritative barrier for
/// capturing the already-enqueued frame/note prefix.
async fn finish_session_rpcs_at_deadline(
    daemon: &Arc<Mutex<Daemon>>,
    tasks: &mut tokio::task::JoinSet<()>,
) {
    while let Some(result) = tasks.try_join_next() {
        if let Err(error) = result {
            eprintln!("agitd: a session RPC worker failed: {error}");
        }
    }
    if tasks.is_empty() {
        return;
    }

    // Freeze the set observed at the deadline. A worker may finish while a
    // failed disk write is backing off; that must not make the next attempt
    // forget a guard whose outcome was unknown when the boundary was crossed.
    let mut hard_stop_guards = None;
    let mut persist_retry = FAIL_CLOSED_PERSIST_RETRY_MIN;
    loop {
        let persist = {
            let mut state = daemon.lock().await;
            let hard_stop_guards = hard_stop_guards
                .get_or_insert_with(|| state.inflight_guard_sensitive_session_rpcs());
            let result = state.persist_fail_closed_session_rpcs(hard_stop_guards);
            if result.is_ok() {
                // Set the abort bit while the state mutex is still held. A
                // worker waiting to project a late result cannot overwrite the
                // durable Plan snapshot between successful persistence and
                // cancellation.
                tasks.abort_all();
            }
            result
        };
        match persist {
            Ok(()) => break,
            Err(error) => {
                eprintln!(
                    "agitd: could not persist fail-closed state for unfinished session RPCs; retaining their gates and retrying: {error:#}"
                );
                tokio::time::sleep(persist_retry).await;
                persist_retry = persist_retry
                    .saturating_mul(2)
                    .min(FAIL_CLOSED_PERSIST_RETRY_MAX);
            }
        }
    }
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            eprintln!("agitd: a session RPC worker failed during shutdown: {error}");
        }
    }
}

/// One guard-sensitive session RPC frozen at the hard shutdown deadline.
///
/// The token is allocated exactly once with the deadline snapshot and reused
/// across every persistence retry. Pairing it with the exact live generation
/// prevents a delayed retry from tightening (and then authorizing cancellation
/// against) a replacement harness under the same logical session id.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HardStopGuard {
    session_id: String,
    generation: u64,
    token: String,
    dangerous: bool,
}

fn fresh_shutdown_guard_token_with(
    guard_attempts: Option<&std::collections::BTreeMap<String, roster::GuardAttempt>>,
    mut next_uuid: impl FnMut() -> String,
) -> String {
    loop {
        let token = format!("{}{}", roster::SHUTDOWN_GUARD_PREFIX, next_uuid());
        if guard_attempts.is_none_or(|attempts| !attempts.contains_key(&token)) {
            return token;
        }
    }
}

fn fresh_shutdown_guard_token(
    guard_attempts: Option<&std::collections::BTreeMap<String, roster::GuardAttempt>>,
) -> String {
    fresh_shutdown_guard_token_with(guard_attempts, || uuid::Uuid::new_v4().to_string())
}

impl WatchLive {
    /// Renew the lease: push "last activity" to now.
    ///
    /// Called when a viewer is added. A method so the production path and the tests share **one**
    /// write site — a test that writes that store by hand proves nothing about the production
    /// code.
    fn renew(&self) {
        self.active
            .store(now_secs(), std::sync::atomic::Ordering::Release);
    }
}

/// Which read-only watches are due to be reaped.
///
/// A free function so the critical instant can be tested: the test itself is a pure function, and
/// its whole meaning is that it and "add a viewer" happen under the same mutex.
fn stale_watches(watches: &HashMap<String, WatchLive>, now: u64) -> Vec<String> {
    let idle_secs = WATCH_IDLE_STOP.as_secs();
    watches
        .iter()
        .filter(|(_, w)| {
            now.saturating_sub(w.active.load(std::sync::atomic::Ordering::Acquire)) >= idle_secs
        })
        .map(|(k, _)| k.clone())
        .collect()
}

/// The "a stretch is missing here" notice, **grown out of the frame that was dropped**.
///
/// # Why it must grow out of that frame
///
/// The notice goes to the hub, and the hub decides whether to fan a frame out with
/// `Frame::is_event()` (a notification, with a stream, and **with a seq**). A
/// `Frame::notification(...)` built out of nowhere has `seq` `None` — on the hub side it is not
/// an event at all and is dropped, so the notice whose whole job is to make the gap visible is
/// itself invisible.
///
/// (The same mechanism catches stripping the numbering off a terminal stream, and local tests
/// stay green either way. So this is a named function with tests that assert `is_event()`, not a
/// line of construction code written in passing.)
///
/// Reusing the dropped frame's `stream` and `seq` is right: this notice takes its **place** at
/// the same position, and a terminal stream takes part in neither deduplication nor replay, so no
/// reader cares whether a number repeats.
fn gap_notice(dropped: &Frame, terminal_id: &str) -> Frame {
    let mut notice = dropped.clone();
    notice.method = Some(method::TERMINAL_OUTPUT.to_string());
    notice.params = terminal_output_frame(
        terminal_id.to_string(),
        "\r\n[agit] the link to the hub stalled; a stretch of terminal output could not be delivered.\r\n"
            .into(),
    )
    .params;
    // It must not be dropped in turn — otherwise "something was dropped" is dropped along with
    // it. **It must not jump the line either**: it inherits the dropped frame's seq, and getting
    // ahead of a smaller seq on the same stream just opens another gap. So it goes through
    // `send_ordered` (queue at the back, wait for capacity) rather than being marked `reliable`
    // for the high-priority queue.
    debug_assert!(
        notice.is_event(),
        "the notice must be a well-formed event; otherwise the hub drops it"
    );
    notice
}

/// Production terminal notifications are serialized from the protocol DTOs,
/// so a serde rename/default/omission change cannot silently drift from the
/// wire emitted by the daemon.
fn terminal_output_frame(terminal_id: String, data: String) -> Frame {
    Frame::notification(
        method::TERMINAL_OUTPUT,
        TerminalOutput { terminal_id, data },
    )
}

fn terminal_exited_frame(terminal_id: String, code: Option<i32>) -> Frame {
    Frame::notification(
        method::TERMINAL_EXITED,
        TerminalExited { terminal_id, code },
    )
}

/// Unix seconds. Used only to subtract for "how long has this tail been quiet", so nothing finer
/// is needed.
pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A session currently under a read-only watch (`session.watch`).
struct WatchLive {
    info: SessionInfo,
    handle: tokio::task::JoinHandle<()>,
    /// When this tail last saw activity (Unix seconds).
    ///
    /// **The daemon decides whether to pack up, not the tail itself.** With the tail deciding,
    /// "it is about to exit" and "someone is about to join" are two independent parties each
    /// looking at their own moment: after the tail reads "no new viewers" and before it really
    /// breaks, a new viewer joins and sees `is_finished()` still false — so they hang off a dying
    /// tail and not one frame arrives.
    ///
    /// With the test inside the daemon, "add a viewer" and "pack up" happen under the same mutex
    /// and that gap does not exist. See `Daemon::reap_idle_watches`.
    active: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Who is watching, and how many times each. **Counted in per-account buckets.**
    ///
    /// Every property here has to hold; drop one and a use breaks:
    ///
    /// * A bare counter: any member's single `unwatch` decrements it, and at zero the whole thing
    ///   is cut off — a colleague's live view stops with no warning, and they did nothing.
    /// * A plain set of accounts: adding becomes idempotent, so **one person opening two tabs**
    ///   never registers the second; closing the first aborts the second's tail along with it,
    ///   and with that `watches` row gone the second tab cannot even resubscribe.
    ///
    /// Bucketed counting holds them at once: a person's tabs each count, and they can **only
    /// decrement their own**.
    viewers: std::collections::BTreeMap<String, usize>,
    /// Which generation this is on the same key.
    ///
    /// A tail task sends a `WatchEnded` note back when it ends, and while that note waits in the
    /// queue a new `session.watch` may already have rebuilt the same key. Comparing generations
    /// on removal is what keeps the freshly built one from being removed.
    generation: u64,
}

/// One open terminal, together with the workspace it belongs to.
///
/// Recording it is required: the params of `terminal.input` / `resize` / `close` carry only a
/// `terminal_id` and no workspace — without this, those three verbs mean "whoever guesses the id
/// can type into that shell". The id is a uuid v4, so this is defense in depth rather than a
/// currently exploitable path, but a shell is worth the depth.
struct TermLive {
    workspace_id: String,
    term: Terminal,
}

/// What comes out of the session gate: the instruction channel, plus the few facts each verb
/// still has to consult.
///
/// The channel is wrapped here instead of letting dispatch fetch it out of `sessions` itself,
/// because "getting the channel" and "you may touch this session" must be one action. Split into
/// two helpers, `turn.start` / `turn.steer` / `turn.interrupt` each do only the "get" half — and
/// then any operator in the same workspace can keep sending messages into a session the owner
/// started with bypass.
struct Driving {
    tx: mpsc::Sender<Command>,
    /// The session's harness. The mode-change path uses it to decide "can this runtime express
    /// that".
    runtime: String,
}

/// The per-session ordering lease acquired before a queued viewer RPC is
/// prepared. A freshly launched harness may not have reported its native
/// thread id yet; in that one case the lease crosses the short `Bound` wait so
/// a later mode change cannot overtake the earlier request.
struct SessionRpcLease {
    session_id: String,
    generation: u64,
    serial: tokio::sync::OwnedMutexGuard<()>,
}

enum SessionRpcPreparation {
    Ready(Box<PreparedSessionRpc>),
    AwaitingDurableGuardRow(SessionRpcLease),
}

struct SessionRpcBoundWait {
    daemon: Arc<Mutex<Daemon>>,
    outbound: crate::rc::outbound::OutboundTx,
    id: crate::protocol::RequestId,
    frame: Frame,
    connection_epoch: u64,
    stop: tokio::sync::watch::Receiver<bool>,
}

struct Live {
    /// Which generation on the same logical id. See `SessionNote::Ended`.
    generation: u64,
    info: SessionInfo,
    tx: mpsc::Sender<Command>,
    /// The harness's own session id. **It exists only in this table** and never goes on the
    /// wire — viewers address sessions by logical session id alone, which is why "continue on
    /// another machine" and "continue on another harness" are transparent to the web interface.
    /// It is kept here for one thing: recognizing "this local session is already supervised", so
    /// a second process does not fight for the same transcript file.
    runtime_thread_id: Option<String>,
    /// This session's task. **Only so shutdown can wait on it** — `Command::Shutdown` merely
    /// enters the queue, and the harness's teardown (SIGTERM → `SHUTDOWN_GRACE_MS` → SIGKILL, see
    /// `Proc::shutdown`) starts only after that. If the daemon exits immediately, those child
    /// processes are orphaned.
    task: tokio::task::JoinHandle<()>,
    /// How many times this session has been "armed" — every dangerous mode change increments it
    /// as it flips `ever_dangerous` from false to true before queueing. The rollback note carries
    /// it back, and only a matching number may flip it back.
    ///
    /// The note is asynchronous: while it travels, the same session may be armed again, and
    /// flipping back then erases the **new** arming — which may really have taken effect.
    danger_arm: u64,
    /// The mode already queued that takes effect only on the **next turn** (codex's `next_turn`).
    ///
    /// It is not decoration. Deciding whether something "loosens" needs a starting point, and
    /// looking only at the **effective** mode makes that starting point a value the owner just
    /// invalidated by hand: the owner queues a switch to `plan` mid-run while
    /// `info.permission_mode` is still the old `auto`; the operator immediately requests
    /// `default`, which against `auto` is tightening and is allowed — and what it overwrites is
    /// that `plan`, so the next turn can write files again.
    ///
    /// So the baseline is the **stricter** of the two, see `authorization_baseline`.
    pending_mode: Option<crate::protocol::PermissionMode>,
    /// Approval id → the global mode a Claude "allow for session" answer
    /// would apply. Populated only from machine-originated approval events;
    /// viewer responses cannot choose or forge the mode being authorized.
    approval_session_modes: HashMap<String, crate::protocol::PermissionMode>,
    /// At most one viewer-originated command may be in flight for this live
    /// session. The guard is acquired with `try_lock_owned` while the daemon's
    /// global mutex is held, then carried by [`PreparedSessionRpc`] across the
    /// queue/reply waits **after** that global mutex has been released.
    ///
    /// This preserves the old per-session ordering/state assumptions without
    /// letting a slow session freeze unrelated sessions or the frame pump.
    rpc_gate: Arc<Mutex<()>>,
    /// The RPC currently owning `rpc_gate` can change the durable permission
    /// guard. Used only at the hard shutdown boundary: an unresolved outcome
    /// must restart as Plan, while a stuck turn/steer/interrupt must not
    /// gratuitously rewrite the user's chosen mode.
    rpc_guard_sensitive: bool,
    /// Late exact confirmations may overtake their viewer RPC workers after
    /// removing durable tokens. Keep each confirmed token until its own
    /// completion validates the CAS; one old receipt must not overwrite another.
    confirmed_turn_guards: std::collections::BTreeMap<String, crate::protocol::PermissionMode>,
    /// The guard token owned by the one viewer RPC currently holding
    /// `rpc_gate`. It lets a late exact confirmation distinguish "my completion
    /// has not projected yet" from an older confirmation arriving during a
    /// newer turn, so the in-memory confirmation latch stays bounded.
    inflight_turn_guard: Option<String>,
    /// Tokens inherited when this harness generation was spawned. They force a
    /// Plan launch and may be cleared only after this generation reports its
    /// authoritative Ready event. Tokens armed after spawn are deliberately not
    /// in this set and therefore cannot be cleared by the startup barrier.
    restart_guard_attempts: std::collections::BTreeSet<String>,
    /// Exact native mode used to launch the recovery generation. Ready may
    /// promote this snapshot—not a mutable later `SessionInfo` projection—to
    /// the durable baseline when it clears inherited attempts.
    restart_guard_mode: Option<crate::protocol::PermissionMode>,
    /// The supervisor has ended, but an accepted RPC still owns `rpc_gate` and
    /// may need to project its result (or roll back a never-run danger arm).
    /// Keep this generation until that RPC finishes; then remove it exactly as
    /// the ordinary `SessionNote::Ended` path would.
    ended: bool,
}

impl Live {
    fn authorization_baseline(&self) -> crate::protocol::PermissionMode {
        authorization_baseline(self.info.permission_mode, self.pending_mode)
    }

    /// Project the authoritative mode facts the supervisor broadcasts back into the daemon's
    /// roster state.
    fn observe_permission_mode(&mut self, p: &crate::protocol::SessionPermissionMode) {
        // NextTurn only means "queued"; the current turn still runs the old mode. It must stay in
        // pending until the supervisor really applies it on the next turn and broadcasts
        // Immediate; otherwise an operator can overwrite the stricter mode the owner just queued
        // with the old effective baseline.
        if p.applied == crate::protocol::PermissionApply::Immediate {
            self.info.permission_mode = Some(p.mode);
            if self.pending_mode == Some(p.mode) {
                self.pending_mode = None;
            }
        }
        // dangerous is a standing fact about the session: even when the loosening takes effect
        // only on the next turn, a daemon restart or another viewer's projection must not forget
        // this owner authorization.
        self.info.dangerous = self.info.dangerous || p.mode.is_dangerous();
    }
}

/// A session command prepared under the daemon mutex and executed after that
/// mutex has been released.
///
/// `serial` is intentionally owned by the request: while it lives, no second
/// command can prepare against stale permission/danger state for the same live
/// generation. Different sessions have different guards and therefore remain
/// independent.
struct PreparedSessionRpc {
    session_id: String,
    generation: u64,
    serial: tokio::sync::OwnedMutexGuard<()>,
    operation: SessionRpcOperation,
}

/// Viewer RPC receipts are cancellation-aware. Graceful daemon shutdown asks
/// every worker to withdraw before the session task is stopped; if a worker is
/// forcibly dropped at the hard deadline, this wrapper still wins the same
/// QUEUED -> ABANDONED CAS as an ordinary timeout. A command that was never
/// accepted therefore cannot run later without a response. Already-taken
/// commands remain taken.
struct SessionReceipt<T>(crate::rc::ticket::Receipt<T>);

impl<T> SessionReceipt<T> {
    fn get_mut(&mut self) -> &mut crate::rc::ticket::Receipt<T> {
        &mut self.0
    }

    async fn wait_until_closed(
        &mut self,
    ) -> Result<crate::Result<T>, tokio::sync::oneshot::error::RecvError> {
        self.0.wait_until_closed().await
    }

    fn abandon(&self) -> crate::rc::ticket::Abandon {
        self.0.abandon()
    }
}

impl<T> Drop for SessionReceipt<T> {
    fn drop(&mut self) {
        let _ = self.0.abandon();
    }
}

enum SessionRpcOperation {
    Turn {
        tx: mpsc::Sender<Command>,
        command: Command,
        reply: SessionReceipt<TurnStartOutcome>,
        guard_attempt: Option<crate::rc::harness::TurnGuardAttempt>,
    },
    Steer {
        tx: mpsc::Sender<Command>,
        command: Command,
        reply: SessionReceipt<crate::protocol::Delivery>,
    },
    SetPermissionMode {
        tx: mpsc::Sender<Command>,
        command: Command,
        reply: SessionReceipt<PermissionModeOutcome>,
        mode: crate::protocol::PermissionMode,
        armed: Option<u64>,
        /// Stable recovery fence reserved before native dispatch. It is not
        /// persisted unless the typed outcome is Unknown.
        recovery_token: String,
    },
    Interrupt {
        tx: mpsc::Sender<Command>,
        command: Command,
        reply: SessionReceipt<()>,
    },
    Approve {
        tx: mpsc::Sender<Command>,
        command: Command,
        reply: SessionReceipt<ApprovalOutcome>,
        approval_id: String,
        danger: DangerAuthorization,
        session_mode: Option<crate::protocol::PermissionMode>,
    },
}

#[derive(Clone)]
enum SessionRpcCompletion {
    None,
    Turn {
        guard_attempt: Option<crate::rc::harness::TurnGuardAttempt>,
        accepted_mode: Option<crate::protocol::PermissionMode>,
        confirmation: Option<TurnStartConfirmation>,
        fail_closed: bool,
        /// The supervisor proved this exact harness generation's process tree
        /// terminated before finishing the typed receipt.
        retire_generation: bool,
    },
    PermissionMode {
        mode: crate::protocol::PermissionMode,
        applied: Option<crate::protocol::PermissionApply>,
        rollback_arm: Option<u64>,
        /// Presence means the supervisor proved the harness tree terminated
        /// after an ambiguous native mode write. The token stays durable until
        /// the next Plan generation reaches Ready.
        recovery_token: Option<String>,
        /// The supervisor proved this exact harness generation's process tree
        /// terminated before finishing the typed receipt.
        retire_generation: bool,
    },
    Approval {
        approval_id: String,
        /// Remove the machine-originated suggestion only once the supervisor
        /// consumed the native approval (success, refusal, or ambiguity).
        resolved: bool,
        effective_mode: Option<crate::protocol::PermissionMode>,
        fail_closed: bool,
        rollback_arm: Option<u64>,
        /// The supervisor proved this exact harness generation's process tree
        /// terminated before finishing the typed receipt.
        retire_generation: bool,
    },
}

impl SessionRpcCompletion {
    fn retires_generation(&self) -> bool {
        match self {
            Self::None => false,
            Self::Turn {
                retire_generation, ..
            }
            | Self::PermissionMode {
                retire_generation, ..
            }
            | Self::Approval {
                retire_generation, ..
            } => *retire_generation,
        }
    }
}

struct ExecutedSessionRpc {
    response: Result<serde_json::Value, RpcError>,
    pending: Option<PendingSessionRpc>,
}

struct PendingSessionRpc {
    session_id: String,
    generation: u64,
    serial: tokio::sync::OwnedMutexGuard<()>,
    operation: PendingSessionRpcOperation,
}

enum PendingSessionRpcOperation {
    Steer(SessionReceipt<crate::protocol::Delivery>),
    Interrupt(SessionReceipt<()>),
    Approve {
        reply: SessionReceipt<ApprovalOutcome>,
        approval_id: String,
        danger: DangerAuthorization,
        session_mode: Option<crate::protocol::PermissionMode>,
    },
}

/// Which mode is the starting point when deciding whether a mode change loosens.
///
/// **The stricter of the two.** A mode already queued but not yet in effect is a promise already
/// made; treating it as absent lets anyone quietly replace it with one request that "looks like
/// tightening" — the owner queues a switch to `plan` mid-run, the operator immediately requests
/// `default`: against the effective `auto` that is tightening, so it is allowed, and what it
/// overwrites is exactly that `plan`, so the next turn can write files again.
///
/// With no effective mode to ask for, the strictest (`plan`) applies: a live session always has a
/// value in this slot (launch and resume both write it), so reaching the backstop is worth one
/// extra request for permission.
fn authorization_baseline(
    effective: Option<crate::protocol::PermissionMode>,
    pending: Option<crate::protocol::PermissionMode>,
) -> crate::protocol::PermissionMode {
    let effective = effective.unwrap_or(crate::protocol::PermissionMode::Plan);
    match pending {
        Some(p) if p.strictness() < effective.strictness() => p,
        _ => effective,
    }
}

/// The line cap on read-only watch backfill: when the web interface opens a session running in
/// someone else's terminal, this many recent lines are filled in first.
const WATCH_BACKFILL_LINES: u64 = 400;

/// The cap on terminals open at once on one machine.
///
/// Each terminal is a real PTY plus a shell process plus a read loop, and what opens it is a
/// button on a web page. This cap sits far above anything a person opens by hand, and it pins the
/// "redeliver in order" tail to a finite requirement.
const MAX_TERMINALS: usize = 32;

/// Frames that may be neither dropped nor reordered while they wait for event queue capacity.
///
/// `pending` is also the per-stream seal: in the window where a frame has been handed to the
/// async tail but the tail has not yet entered the event FIFO, a later, larger seq on the same
/// stream must not jump the line through `try_send`. Such frames are dropped as congested output
/// and stay in the journal for replay; terminal output adds a gap notice where needed, while
/// `terminal.exited` and connection-bound `commit.settled` keep entering this FIFO.
///
/// The channel itself is unbounded, but production has a structural cap: the first pending frame
/// pauses `terminal.open`, and each already-open terminal contributes at most one gap and one
/// exit. So "the queue is full" is never a reason to drop a terminal state, and repeated
/// open/exit during a disconnection cannot blow memory up.
struct OrderedTail {
    tx: mpsc::UnboundedSender<Frame>,
    pending: std::collections::BTreeMap<String, usize>,
    admission_blockers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl OrderedTail {
    fn new(
        tx: mpsc::UnboundedSender<Frame>,
        admission_blockers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) -> Self {
        Self {
            tx,
            pending: Default::default(),
            admission_blockers,
        }
    }

    fn seals(&self, frame: &Frame) -> bool {
        frame
            .stream
            .as_deref()
            .is_some_and(|stream| self.pending.contains_key(stream))
    }

    /// `reserved` is only for an exit the terminal reader pre-registered; a gap takes its own
    /// slot here.
    fn enqueue(&mut self, frame: Frame, reserved: bool) -> Result<(), ()> {
        let stream = frame.stream.clone().unwrap_or_default();
        let terminal = stream.starts_with("term:");
        debug_assert!(
            !reserved || terminal,
            "only terminal exits pre-reserve a slot"
        );
        if terminal {
            if !reserved {
                self.admission_blockers
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            } else {
                debug_assert!(
                    self.admission_blockers
                        .load(std::sync::atomic::Ordering::Acquire)
                        > 0,
                    "a terminal exit must reserve a delivery blocker before releasing the live slot"
                );
            }
        }
        *self.pending.entry(stream.clone()).or_default() += 1;
        if self.tx.send(frame).is_err() {
            self.acknowledge(&stream);
            return Err(());
        }
        Ok(())
    }

    fn acknowledge(&mut self, stream: &str) {
        let Some(left) = self.pending.get_mut(stream) else {
            debug_assert!(
                false,
                "a tail ack must have a matching pending stream: {stream}"
            );
            return;
        };
        *left -= 1;
        if *left == 0 {
            self.pending.remove(stream);
        }
        if stream.starts_with("term:") {
            let before = self
                .admission_blockers
                .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            debug_assert!(before > 0, "terminal delivery blocker underflow");
        }
    }

    fn reserve_terminal_exit(blockers: &std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        blockers.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    fn terminal_exit(frame: &Frame) -> bool {
        frame.method() == method::TERMINAL_EXITED
            && frame
                .stream
                .as_deref()
                .is_some_and(|stream| stream.starts_with("term:"))
    }

    fn must_order(frame: &Frame) -> bool {
        Self::terminal_exit(frame) || frame.connection_delivery.is_some()
    }

    fn should_defer(&self, frame: &Frame) -> bool {
        Self::terminal_exit(frame) || self.seals(frame)
    }

    fn reserved_by_reader(frame: &Frame) -> bool {
        Self::terminal_exit(frame)
    }

    fn blockers(blockers: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> usize {
        blockers.load(std::sync::atomic::Ordering::Acquire)
    }

    fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

fn terminal_delivery_blocked(blockers: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> bool {
    OrderedTail::blockers(blockers) > 0
}

fn send_live_frame(
    tx: &crate::rc::outbound::OutboundTx,
    tail: &OrderedTail,
    frame: Frame,
) -> crate::rc::outbound::Sent {
    if tail.should_defer(&frame) {
        crate::rc::outbound::Sent::DroppedReplayable(Box::new(frame))
    } else {
        tx.send(frame)
    }
}

/// Whether the shared frame pump must pass this frame through session
/// projection before it can reach any outbound lane.
///
/// A generation tag always wins over the usual “already has seq” replay
/// shortcut. Tagged frames only come from live supervisors; a pre-numbered or
/// otherwise malformed one must reach the fence so it can be invalidated and
/// dropped rather than impersonating replay.
fn needs_session_projection(frame: &Frame) -> bool {
    frame.source_generation.is_some()
        || (frame.is_notification() && frame.stream.is_some() && frame.seq.is_none())
}

fn tag_session_frame(frame: &mut Frame, session_id: &str, generation: u64) {
    frame.stream = Some(session_id.to_string());
    frame.source_generation = Some(generation);
}

/// The exact already-enqueued prefix that must be projected after the last
/// admitted session RPC worker exits.
///
/// Session supervisors enqueue permission/danger facts before closing the RPC
/// receipt. Therefore `JoinSet::is_empty()` is the producer-side barrier: once
/// it is true, `Receiver::len()` captures every tail item causally owned by
/// those RPCs. Drain that fixed prefix instead of waiting for the channels to
/// become empty (live sessions can keep producing unrelated output forever).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShutdownProjectionTail {
    frames: usize,
    notes: usize,
}

impl ShutdownProjectionTail {
    fn capture(frames: &mpsc::Receiver<Frame>, notes: &mpsc::Receiver<SessionNote>) -> Self {
        Self {
            frames: frames.len(),
            notes: notes.len(),
        }
    }

    fn took_frame(&mut self) {
        debug_assert!(self.frames > 0, "drained beyond the captured frame prefix");
        self.frames = self.frames.saturating_sub(1);
    }

    fn took_note(&mut self) {
        debug_assert!(self.notes > 0, "drained beyond the captured note prefix");
        self.notes = self.notes.saturating_sub(1);
    }

    fn complete(self) -> bool {
        self.frames == 0 && self.notes == 0
    }
}

/// The cap on subscription replays in flight at once. See [`Daemon::replay_slots`].
///
/// `REPLAY_SLOTS` × `REPLAY_CAP` frames is the memory ceiling on this path. Ordinary use runs one
/// or two at a time (opening a page, switching sessions).
const REPLAY_SLOTS: usize = 4;

/// How long a transcript stays quiet before a read-only watch packs up on its own.
///
/// A viewer may leave by closing the tab, so `session.unwatch` never arrives. Without this
/// backstop, every session ever watched leaves a permanent file poll on this machine. A session
/// with nothing new after that long no longer counts as "running", and watching it again costs
/// one more `session.watch`.
const WATCH_IDLE_STOP: std::time::Duration = std::time::Duration::from_secs(600);

/// The cap on each leg of waiting on a session instruction. See [`reply_within_state`].
///
/// Far wider than an ordinary settlement (the transcript-quiet decision
/// `supervisor::SETTLE_MAX_MS` plus one local commit), and far shorter than a git push to an
/// unreachable remote (that is the TCP timeout, minutes). At worst an RPC queues for one leg,
/// waits for acceptance/result for another, and waits again once accepted, each leg capped by
/// this; all of them hold only the target session's gate, never the daemon's global mutex. When
/// the receipt is still missing after the second leg, the answer is outcome unknown while a
/// background worker keeps the gate until execution really ends, so the next instruction cannot
/// be prepared against stale state.
const SESSION_REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(12);
/// The hub relays a viewer request for a bounded window; this constant mirrors
/// that hub-side deadline. Keep the machine-side budget explicit here so a
/// local pre-dispatch wait cannot silently consume the receipt window that
/// follows it.
#[cfg(test)]
const HUB_RELAY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A guard-sensitive command issued immediately after `session.start` waits
/// for the harness's authoritative `Bound` note instead of returning a false
/// "runtime unavailable"-looking 303. The waiter owns only the target
/// session's gate; each poll releases the daemon mutex so the `Bound` note can
/// itself be projected.
// Codex may restart its app-server once while loading native state, which pushes Bound well past
// the request that needs it; this budget is sized to absorb one such restart. The full ordinary
// path may then spend three SESSION_REPLY_TIMEOUT phases queueing, waiting for acceptance, and
// waiting for an accepted result. HUB_RELAY_TIMEOUT bounds all four waits together, and
// `durable_guard_bind_wait_preserves_the_complete_hub_rpc_window` pins the headroom left over.
const DURABLE_GUARD_BIND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const DURABLE_GUARD_BIND_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Graceful-stop budget for admitted viewer RPCs.
///
/// An instruction that was never taken withdraws itself as soon as the stop
/// signal arrives. A TAKEN instruction must keep its receipt/generation fence
/// until the executor reports a result, because dropping that projection can
/// resurrect a looser roster mode after restart. The ordinary worst case is
/// three reply windows; anything still live after this budget is durably
/// forced to Plan before its worker is aborted.
const SESSION_RPC_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(37);

/// The cap on how long the **whole fleet of sessions** gets to finish its exit work at shutdown.
///
/// One exit settlement spends `SETTLEMENT_DELIVERY_WAIT` just waiting for the hub to accept
/// `commit.settled`, and land / commit / push come after that. A cap that cannot hold one full
/// settlement guarantees this path is cut in half once it really starts: local git holds the
/// trailing turn, the hub does not.
/// This stage runs in parallel (notify all, then wait together), so the cap covers **one**
/// settlement and does not grow with the number of sessions.
const FLEET_EXIT_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// A hard-stop guard must become durable before its worker can be cancelled.
/// Disk errors can be transient (for example, a short-lived antivirus or
/// filesystem lock), so keep the gates and retry instead of exiting into a
/// restart that would load the older, looser mode.
const FAIL_CLOSED_PERSIST_RETRY_MIN: std::time::Duration = std::time::Duration::from_millis(250);
const FAIL_CLOSED_PERSIST_RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(5);

pub struct Daemon {
    /// Replay frames this dispatch accumulated, to be sent **outside the mutex**. See
    /// [`Daemon::on_frame`].
    deferred: Vec<Frame>,
    /// The slot for those frames, handed to the send task together with them.
    deferred_slot: Option<tokio::sync::OwnedSemaphorePermit>,
    /// The cap on subscription replays in flight at once.
    ///
    /// Every replay holds a copy of up to `REPLAY_CAP` frames until all of them have entered the
    /// outbound queue. Bounding the queue is not enough on its own: once it is full these tasks
    /// merely suspend, and the task and the `Vec<Frame>` it holds are still there — a page that
    /// reconnects over and over piles them up anyway.
    ///
    /// No slot means **refusing on the spot**: `session.subscribe` can be retried, a room full of
    /// suspended tasks cannot.
    replay_slots: std::sync::Arc<tokio::sync::Semaphore>,
    /// The outbound queue's sender, **for replay only**.
    ///
    /// Every other frame goes `frames_tx` → main loop → here (the main loop numbers them and
    /// records them in the journal). Replay frames need neither — they **came out of the
    /// journal** and already carry their numbers — and they must go through the waiting
    /// `send_replay`, which is what the main loop cannot do (one wait stops everything).
    outbound: Option<crate::rc::outbound::OutboundTx>,
    opts: Options,
    journal: Journal,
    mirror: Mirror,
    /// The **durable** map from logical session id → (runtime, thread id, cwd, lineage).
    /// The in-memory `sessions` dies with the daemon; without this file every historical session
    /// row in the web interface becomes a dead link after a restart — clicking one reports
    /// "no local session".
    roster: Roster,
    sessions: HashMap<String, Live>,
    /// Logical session id → newest harness generation whose launch completed.
    ///
    /// This is a process-lifetime tombstone, not a mirror of `sessions`: Ended
    /// and failed-bootstrap cleanup remove the live handle but deliberately
    /// retain this fence so a delayed frame from that generation cannot become
    /// current again after a newer generation has materialized and ended.
    latest_session_generations: HashMap<String, u64>,
    /// Read-only watch (`session.watch`) tail tasks, registered by watch stream id.
    /// `info` is kept so `session.subscribe` can subscribe to a watch stream too (replaying its
    /// ring).
    watches: HashMap<String, WatchLive>,
    /// The terminals open in the bottom-right corner. A sibling of sessions rather than nested
    /// inside them: a terminal belongs to no session — it is a person typing on this machine.
    terminals: HashMap<String, TermLive>,
    /// While a terminal gap/exit has not yet entered the outbound FIFO, no new terminal is
    /// admitted.
    terminal_delivery_blockers: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Where terminal bytes flow back. Built when the first terminal opens.
    term_tx: Option<mpsc::Sender<TerminalEvent>>,
    online: bool,
    connection_id: Option<String>,
    /// Shared by every live session; a control socket reload builds the replacement in full and
    /// swaps it in at once.
    secret_filter: crate::domain::secret_filter::MatcherHandle,
    /// Live authorization for RC repository settlement. See
    /// [`SettlementState`]; every transition invalidates the previous epoch.
    settlement: tokio::sync::watch::Sender<SettlementState>,
    started_at: std::time::Instant,
    /// The internal session → daemon reporting channel (harness id and the like; never on the
    /// wire).
    notes: mpsc::Sender<SessionNote>,
    /// The command names an owner grants an operator to answer on their own. See
    /// [`crate::rc::grants`].
    grants: crate::rc::grants::Grants,
    /// The generation counter for read-only watches. See [`WatchLive::generation`].
    watch_generation: u64,
    /// The generation counter for sessions. See [`Live::generation`].
    session_generation: u64,
    /// workspace_id → that workspace's confinement **right now**, broadcast to every session
    /// under it.
    ///
    /// A session holds a `watch::Receiver`, so unbinding a directory or revoking a grant takes
    /// effect on **running** sessions immediately. If each session copied the roots at launch and
    /// kept that copy for life, the moment the test flips that stale copy is a long-lived pass.
    confinement: HashMap<String, tokio::sync::watch::Sender<crate::rc::Confinement>>,
}

async fn complete_prepared_session_rpc(
    daemon: Arc<Mutex<Daemon>>,
    session_id: String,
    generation: u64,
    serial: tokio::sync::OwnedMutexGuard<()>,
    completion: SessionRpcCompletion,
) {
    let mut retry = FAIL_CLOSED_PERSIST_RETRY_MIN;
    loop {
        let result = {
            let mut daemon = daemon.lock().await;
            daemon.complete_session_rpc(&session_id, generation, &completion)
        };
        match result {
            Ok(()) => break,
            Err(error) => {
                eprintln!(
                    "agitd: could not durably project a completed session guard; retaining its RPC gate and retrying: {error:#}"
                );
                // The global daemon mutex is deliberately free during
                // backoff. Other sessions and the frame/note pumps continue;
                // only this exact generation remains serialized by `serial`.
                tokio::time::sleep(retry).await;
                retry = retry.saturating_mul(2).min(FAIL_CLOSED_PERSIST_RETRY_MAX);
            }
        }
    }
    // Keep the per-session gate through the state projection and any deferred
    // Ended cleanup. Releasing it sooner lets a new request prepare against
    // stale state or lets Ended delete the Live before completion can fence it.
    drop(serial);
}

async fn release_unprepared_session_rpc(daemon: Arc<Mutex<Daemon>>, lease: SessionRpcLease) {
    complete_prepared_session_rpc(
        daemon,
        lease.session_id,
        lease.generation,
        lease.serial,
        SessionRpcCompletion::None,
    )
    .await;
}

async fn finish_prepared_session_rpc(
    daemon: Arc<Mutex<Daemon>>,
    session_id: String,
    generation: u64,
    serial: tokio::sync::OwnedMutexGuard<()>,
    completion: SessionRpcCompletion,
    response: Result<serde_json::Value, RpcError>,
) -> ExecutedSessionRpc {
    complete_prepared_session_rpc(daemon, session_id, generation, serial, completion).await;
    ExecutedSessionRpc {
        response,
        pending: None,
    }
}

fn project_turn_start_outcome(
    outcome: TurnStartOutcome,
    guard_attempt: Option<crate::rc::harness::TurnGuardAttempt>,
) -> (SessionRpcCompletion, Result<serde_json::Value, RpcError>) {
    match outcome {
        TurnStartOutcome::Accepted {
            turn_id,
            consumed_mode,
            confirmation,
            ..
        } => {
            let expected_mode = guard_attempt
                .as_ref()
                .map(|attempt| attempt.expected_mode);
            if consumed_mode != expected_mode {
                (
                    SessionRpcCompletion::Turn {
                        guard_attempt,
                        accepted_mode: None,
                        confirmation: None,
                        fail_closed: true,
                        retire_generation: false,
                    },
                    Err(RpcError::new(
                        ErrorCode::Internal,
                        format!(
                            "the typed turn result reported consumed mode {consumed_mode:?}, but its durable guard expected {expected_mode:?}"
                        ),
                    )
                    .with_hint(
                        "the harness has been retired; resume it in fail-closed Plan mode",
                    )),
                )
            } else {
                (
                    SessionRpcCompletion::Turn {
                        guard_attempt,
                        accepted_mode: consumed_mode,
                        confirmation: Some(confirmation),
                        fail_closed: false,
                        retire_generation: false,
                    },
                    Ok(serde_json::to_value(TurnStartResult { turn_id }).unwrap()),
                )
            }
        }
        TurnStartOutcome::ExplicitRefusal { message, .. } => (
            SessionRpcCompletion::Turn {
                guard_attempt,
                accepted_mode: None,
                confirmation: None,
                fail_closed: false,
                retire_generation: false,
            },
            Err(RpcError::new(ErrorCode::RuntimeUnavailable, message).with_hint(
                "the harness explicitly rejected the turn; no prompt or queued mode was applied",
            )),
        ),
        TurnStartOutcome::RetryableNotAccepted { message } => (
            SessionRpcCompletion::Turn {
                guard_attempt,
                accepted_mode: None,
                confirmation: None,
                fail_closed: false,
                retire_generation: false,
            },
            Err(RpcError::new(ErrorCode::SessionBusy, message)
                .with_hint("nothing was sent to the harness, so retrying after it is ready is safe")),
        ),
        TurnStartOutcome::ConcurrentNotAccepted { message } => (
            SessionRpcCompletion::Turn {
                guard_attempt,
                accepted_mode: None,
                confirmation: None,
                fail_closed: false,
                retire_generation: false,
            },
            Err(RpcError::new(ErrorCode::SessionBusy, message).with_hint(
                "this RPC sent nothing, but another prompt may already start; wait for its turn event before sending more input",
            )),
        ),
        TurnStartOutcome::FatalNotAccepted { message } => (
            SessionRpcCompletion::Turn {
                guard_attempt,
                accepted_mode: None,
                confirmation: None,
                fail_closed: false,
                retire_generation: true,
            },
            Err(RpcError::new(ErrorCode::RuntimeUnavailable, message).with_hint(
                "nothing was sent, but this harness generation was retired; resume the session before retrying",
            )),
        ),
        TurnStartOutcome::Unknown {
            message,
            attempted_mode,
        } => (
            SessionRpcCompletion::Turn {
                guard_attempt: guard_attempt.clone(),
                accepted_mode: None,
                confirmation: None,
                fail_closed: guard_attempt.is_some() || attempted_mode.is_some(),
                retire_generation: true,
            },
            Err(RpcError::new(ErrorCode::Internal, message).with_hint(
                "the live harness was terminated because retrying could duplicate the prompt; resume the session before sending more input",
            )),
        ),
    }
}

fn project_permission_mode_outcome(
    outcome: PermissionModeOutcome,
    mode: crate::protocol::PermissionMode,
    armed: Option<u64>,
    recovery_token: String,
) -> (SessionRpcCompletion, Result<serde_json::Value, RpcError>) {
    match outcome {
        PermissionModeOutcome::Applied { applied } => (
            SessionRpcCompletion::PermissionMode {
                mode,
                applied: Some(applied),
                rollback_arm: None,
                recovery_token: None,
                retire_generation: false,
            },
            Ok(serde_json::to_value(crate::protocol::SessionSetPermissionModeResult {
                mode,
                applied,
            })
            .unwrap()),
        ),
        PermissionModeOutcome::ExplicitRefusal { message } => (
            SessionRpcCompletion::PermissionMode {
                mode,
                applied: None,
                rollback_arm: armed,
                recovery_token: None,
                retire_generation: false,
            },
            Err(RpcError::new(ErrorCode::RuntimeUnavailable, message).with_hint(
                "the harness explicitly refused the mode change; its native policy was not changed",
            )),
        ),
        PermissionModeOutcome::Unknown { message } => (
            SessionRpcCompletion::PermissionMode {
                mode,
                applied: None,
                rollback_arm: None,
                recovery_token: Some(recovery_token),
                retire_generation: true,
            },
            Err(RpcError::new(ErrorCode::Internal, message).with_hint(
                "the harness was terminated and will resume in fail-closed Plan mode because the native policy may already have changed",
            )),
        ),
    }
}

fn map_approval_reply(result: Result<(), RpcError>) -> Result<serde_json::Value, RpcError> {
    match result {
        Ok(()) => Ok(serde_json::json!({})),
        Err(error) if error.code == ErrorCode::Internal as i32 => {
            Err(RpcError::new(ErrorCode::ApprovalExpired, error.message)
                .with_hint("it may have timed out or already been answered"))
        }
        Err(error) => Err(error),
    }
}

fn project_approval_outcome(
    outcome: ApprovalOutcome,
    approval_id: String,
    trusted_mode: Option<crate::protocol::PermissionMode>,
    danger: DangerAuthorization,
) -> Option<(SessionRpcCompletion, Result<serde_json::Value, RpcError>)> {
    match outcome {
        ApprovalOutcome::Applied { effective_mode } => {
            // Both values originate on the machine but travel through
            // different layers. Refuse to ACK if they ever diverge; retaining
            // the gate until hard-stop Plan persistence is safer than writing
            // either untrusted projection into the roster.
            if effective_mode != trusted_mode {
                return None;
            }
            Some((
                SessionRpcCompletion::Approval {
                    approval_id,
                    resolved: true,
                    effective_mode,
                    fail_closed: false,
                    rollback_arm: None,
                    retire_generation: false,
                },
                Ok(serde_json::json!({})),
            ))
        }
        ApprovalOutcome::ExplicitRefusal { message, retained } => Some((
            SessionRpcCompletion::Approval {
                approval_id,
                resolved: !retained,
                effective_mode: None,
                fail_closed: false,
                rollback_arm: danger.arm(),
                retire_generation: false,
            },
            Err(RpcError::new(ErrorCode::ApprovalExpired, message)
                .with_hint("it may have timed out or already been answered")),
        )),
        ApprovalOutcome::Unknown {
            message,
            attempted_mode,
        } => Some((
            SessionRpcCompletion::Approval {
                approval_id,
                resolved: true,
                effective_mode: None,
                fail_closed: trusted_mode.is_some() || attempted_mode.is_some(),
                rollback_arm: None,
                retire_generation: true,
            },
            Err(RpcError::new(ErrorCode::Internal, message).with_hint(
                if trusted_mode.is_some() || attempted_mode.is_some() {
                    "the live harness was terminated and will resume in Plan because its session policy may already have changed"
                } else {
                    "the harness was terminated because the native approval response may have been consumed; resume the session before sending more input"
                },
            )),
        )),
    }
}

/// Whether this frame may **loosen the guard to** `mode`.
///
/// The test is [`PermissionMode::loosens_guard`], not `is_dangerous`: the two differ by
/// `accept_edits` and `auto`, and in those modes claude-code **stops sending `can_use_tool`
/// altogether** — the approval classifier is never called once. Under a test of "allow anything
/// that is not bypass", one `session.setPermissionMode(accept_edits)` frame from an operator goes
/// around the whole fail-closed classification, and that frame is itself entirely legal.
///
/// Tightening is open to anyone at any time: "you need permission to become safer" makes no
/// sense.
fn require_owner_to_loosen(
    caller: Option<&crate::protocol::CallerClaim>,
    mode: crate::protocol::PermissionMode,
    // Which mode is current. **Loosening is relative** — `plan → default` is visible only by
    // comparison, and the whole point of `plan` is "look, do not touch". See
    // `PermissionMode::loosens_from`.
    //
    // `None` = there is no "current" yet (starting a new session): only the absolute test holds
    // then — going from nothing to something is not loosening, but asking for `bypass` from the
    // start is still the owner's business.
    from: Option<crate::protocol::PermissionMode>,
) -> Result<(), RpcError> {
    let loosening = match from {
        Some(from) => mode.loosens_from(from),
        None => mode.loosens_guard(),
    };
    require_owner_if_loosening(caller, loosening)
}

/// The same thing, but with **the conclusion already computed**.
///
/// The mode-change path compares strictness twice: once to decide whether this frame goes through
/// the `Drive` or the `Brake` gate, once to decide whether it takes an owner. Computed
/// separately, `bypass → auto` comes out as both "tightening" (authorization allows it) and
/// "loosening" (routed to `Drive`), so it hits the first gate — two rulers on one thing,
/// disagreeing. Compute it once and use it in both places.
fn require_owner_if_loosening(
    caller: Option<&crate::protocol::CallerClaim>,
    loosening: bool,
) -> Result<(), RpcError> {
    if !loosening || caller.is_some_and(|c| c.is_owner()) {
        return Ok(());
    }
    Err(RpcError::new(
        ErrorCode::DangerousSessionLocked,
        "only the owner can loosen how much this session asks before it acts",
    )
    .with_hint(
        "in these modes the agent stops asking before it edits or runs things — that stays with the workspace owner",
    ))
}

/// Whether this frame may **drive** this session (send a message, steer, allow an approval).
///
/// The test is only that **monotonic** bit: whether this session was ever handed over
/// (`bypass`). Its context may still hold what it read unsupervised at that moment, so for the
/// rest of its life only the owner may drive it.
///
/// # Why the current mode is not consulted here
///
/// Consulting it would mean "this session runs in `accept_edits` ⇒ only the owner may talk to
/// it" — and `accept_edits` / `auto` are **ordinary working modes**; an owner puts a session
/// there precisely so collaborators can push it along. Conflate "switching into this mode takes
/// an owner" with "working in this mode takes an owner" into one test, and the second sentence
/// shuts off this feature's common use entirely.
///
/// The two live in two functions so they cannot be written as one sentence again.
fn require_owner_to_drive(
    caller: Option<&crate::protocol::CallerClaim>,
    session_was_dangerous: bool,
) -> Result<(), RpcError> {
    if !session_was_dangerous || caller.is_some_and(|c| c.is_owner()) {
        return Ok(());
    }
    Err(RpcError::new(
        ErrorCode::DangerousSessionLocked,
        "this session has run without permission checks, so only its owner may drive it",
    )
    .with_hint(
        "running with no checks is handing out an unsupervised shell on a real machine — what it read then is still in its context",
    ))
}

/// `SessionBusy`: a session still open in someone else's terminal cannot be taken over (writing
/// destroys the history; read it with watch).
fn busy_error() -> RpcError {
    RpcError::new(
        ErrorCode::SessionBusy,
        "that conversation is still open in a terminal on this machine",
    )
    .with_hint(
        "resuming it now would start a second writer on the same transcript and corrupt both histories — quit it there first, then take it over (or watch it read-only)",
    )
}

/// `owner/name@branch` → (agent name, branch).
/// The minimum role a verb requires.
///
/// # Why this table also lives on the machine
///
/// `caller_scope` checks only the tenant ("is the workspace you named your own"), never the role.
/// So a request carrying a legitimate **viewer** claim reaches `fs.readDirectory` (owner-only in
/// the protocol; it lists the home directory and has nothing to do with any workspace),
/// `project.bind` (adds a directory to the allowlist) and `terminal.open` (opens a real PTY on
/// this machine). Those three are gated on the hub alone — and the trust model says the hub is a
/// relay, not an authority: one miswritten relay turns a read-only member into an operator of
/// this machine.
///
/// The table lists the **minimum role** rather than "who may not do what": a new verb whose
/// registration is forgotten lands in `Owner` (see the catch-all in `min_role`), and the worst
/// outcome is "nobody but the owner can use it" instead of "anyone can". The direction is
/// deliberate.
#[derive(Clone, Copy, PartialEq)]
enum Role {
    Viewer,
    Operator,
    Owner,
}

fn min_role(method_name: &str) -> Role {
    match method_name {
        // Read-only: see what is on this machine.
        method::WORKSPACE_LIST | method::SESSION_LIST | method::SESSION_SUBSCRIBE => Role::Viewer,
        // A read-only watch **is a read**: it tails a transcript and writes back not one byte.
        // The hub deliberately keeps these two verbs outside the operator gate (its comment says
        // that blocking a viewer only turns "that session is open" into a guessing game). Calling
        // them operator here makes the machine refuse requests the hub explicitly means to
        // allow — the cost of the two layers disagreeing is a viewer looking at a button that
        // does nothing.
        //
        // It does start a tail task, but that cost is held down by the `viewers` count and
        // `WATCH_IDLE_STOP`, not by roles.
        method::SESSION_WATCH | method::SESSION_UNWATCH => Role::Viewer,
        // Drive a session. Each also has its own danger gate (see `session_channel`).
        method::SESSION_START
        | method::SESSION_RESUME
        | method::TURN_START
        | method::TURN_STEER
        | method::TURN_INTERRUPT
        | method::APPROVAL_DECIDE
        | method::SESSION_SET_PERMISSION_MODE
        | method::FS_READ_FILE => Role::Operator,
        // **Acting on the machine itself**: list the home directory (unrelated to any
        // workspace), add a directory to the allowlist, open a real shell.
        method::FS_READ_DIRECTORY
        | method::PROJECT_BIND
        | method::PROJECT_UNBIND
        | method::TERMINAL_OPEN
        | method::TERMINAL_INPUT
        | method::TERMINAL_RESIZE
        | method::TERMINAL_CLOSE => Role::Owner,
        // An unregistered verb takes the strictest role.
        _ => Role::Owner,
    }
}

fn is_queued_session_rpc(method_name: &str) -> bool {
    matches!(
        method_name,
        method::TURN_START
            | method::TURN_STEER
            | method::SESSION_SET_PERMISSION_MODE
            | method::TURN_INTERRUPT
            | method::APPROVAL_DECIDE
    )
}

fn needs_claude_restart_guard_barrier(
    runtime: &str,
    restart_guard_attempts: &std::collections::BTreeSet<String>,
) -> bool {
    runtime == "claude-code" && !restart_guard_attempts.is_empty()
}

fn queued_session_id(f: &Frame) -> Result<String, RpcError> {
    match f.method() {
        method::TURN_START => Ok(f.params_as::<TurnStart>()?.session_id),
        method::TURN_STEER => Ok(f.params_as::<TurnSteer>()?.session_id),
        method::SESSION_SET_PERMISSION_MODE => Ok(f
            .params_as::<crate::protocol::SessionSetPermissionMode>()?
            .session_id),
        method::TURN_INTERRUPT => Ok(f.params_as::<TurnInterrupt>()?.session_id),
        method::APPROVAL_DECIDE => Ok(f
            .params_as::<crate::protocol::ApprovalResponse>()?
            .session_id),
        _ => Err(RpcError::new(
            ErrorCode::Internal,
            "not a queued session instruction",
        )),
    }
}

/// Whether this frame's role is enough for this verb.
fn require_role(caller: &crate::protocol::CallerClaim, method_name: &str) -> Result<(), RpcError> {
    let need = min_role(method_name);
    let allowed = match need {
        Role::Viewer => true,
        Role::Operator => caller.can_operate(),
        Role::Owner => caller.is_owner(),
    };
    if allowed {
        return Ok(());
    }
    let what = match need {
        Role::Owner => "only the owner of this workspace can do that on this machine",
        _ => "you have read-only access to this workspace",
    };
    Err(RpcError::new(ErrorCode::Forbidden, what).with_hint(
        "the hub gates this too, but the machine is the trust boundary and checks again",
    ))
}

/// Queue an instruction onto a session's queue **without waiting indefinitely**.
///
/// # Why not just `send().await`
///
/// That channel is bounded, and a session consumes no instructions while it settles (inside
/// `settle_and_push` there may be a `git push` to an unreachable remote, and the TCP timeout is
/// on the order of minutes). Once the queue is full, `send().await` hangs there. This wait is
/// therefore bounded, and happens only under the **per-session** guard held by
/// [`PreparedSessionRpc`]; the daemon's global mutex is released before the call, so other
/// sessions and the frame pump keep running.
///
/// Failing to queue says plainly "this session is busy". That is true, and this instruction
/// **really did not run even once** — as when the withdrawal wins the CAS, the caller can retry
/// safely.
async fn enqueue_within(
    tx: &mpsc::Sender<Command>,
    cmd: Command,
    within: std::time::Duration,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<(), RpcError> {
    // Reserve first so cancellation retains ownership of `cmd`: dropping a
    // `send(cmd)` future can hide whether the message crossed into the queue.
    // With a permit the stop branch proves it did not, and SessionReceipt's
    // QUEUED -> ABANDONED transition prevents later execution.
    let reserve = tokio::time::timeout(within, tx.reserve());
    tokio::pin!(reserve);
    match tokio::select! {
        biased;
        _ = rpc_stop_requested(stop) => None,
        result = &mut reserve => Some(result),
    } {
        None => Err(
            RpcError::new(ErrorCode::SessionBusy, "this daemon is stopping")
                .with_hint("nothing was queued, so the instruction did not run"),
        ),
        Some(Ok(Ok(permit))) => {
            permit.send(cmd);
            Ok(())
        }
        // The channel is closed: the session is gone.
        Some(Ok(Err(_))) => Err(RpcError::new(
            ErrorCode::SessionNotFound,
            "that session ended",
        )),
        Some(Err(_)) => Err(RpcError::new(
            ErrorCode::SessionBusy,
            "that session's queue is full — it is busy settling a turn",
        )
        .with_hint("nothing was queued, so it is safe to try again")),
    }
}

/// Wait for a session to answer, **but not forever**.
///
/// # Why there must be a cap
///
/// The session's instruction loop takes no instructions while it settles a turn:
/// `settle_transcript` waits at most for `supervisor::SETTLE_MAX_MS`, and `settle_and_push` then
/// spawns `agit commit` and `agit push` subprocesses — git has no timeout, and a push to an
/// unreachable remote waits out the TCP timeout, which is **minutes**.
///
/// This wait holds only the target session's serial gate. Later requests on the same session get
/// `SessionBusy` immediately; other sessions, event numbering, reconnect and watermark
/// persistence do not go through this gate.
///
/// # The timeout measures queueing, not execution
///
/// An instruction has two possible fates, and **only one of them pairs with telling the user
/// "safe to retry"**:
///
/// * **Not taken yet**: the withdrawal won the CAS, so this instruction never ran and never will.
///   Only then may the answer be `SessionBusy` and "nothing happened".
/// * **Already taken**: side effects are on their way and withdrawal no longer applies. The only
///   option is to wait one more leg; if that also runs out, **say so plainly** — "started,
///   outcome unknown". An answer of "safe to retry" makes the caller redo a mode change that
///   already started.
///
/// The test is `rc::ticket`'s three-state atomic: the executor CASes "queued → taken" as it
/// dequeues, this side CASes "queued → withdrawn" on timeout, and whoever wins decides. Reading
/// `reply.is_closed()` leaves a gap — the caller can give up exactly between the executor's check
/// and the start of execution, so it believes nothing ran while it did.
enum ReplyWait<T> {
    Done(Result<T, RpcError>),
    /// The ticket was accepted, but its executor did not finish inside the
    /// second response window. The RPC may return "outcome unknown", while the
    /// same tracked worker keeps the per-session serial gate until the ticket
    /// actually resolves or closes.
    InFlight(RpcError),
}

enum ReceiptPhase<T> {
    Reply(Result<T, RpcError>),
    Timeout,
    Stopping,
}

async fn rpc_stop_requested(stop: &mut tokio::sync::watch::Receiver<bool>) {
    if *stop.borrow() {
        return;
    }
    // The sender is run-local and normally publishes true. Treat it being
    // dropped as stop too; a worker must never wait on a coordinator that no
    // longer exists.
    let _ = stop.changed().await;
}

fn map_receipt<T>(
    got: Result<crate::Result<T>, tokio::sync::oneshot::error::RecvError>,
) -> Result<T, RpcError> {
    match got {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(RpcError::new(ErrorCode::Internal, error.to_string())),
        Err(_) => Err(RpcError::new(ErrorCode::Internal, "session went away")),
    }
}

async fn receipt_phase<T>(
    r: &mut crate::rc::ticket::Receipt<T>,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) -> ReceiptPhase<T> {
    tokio::select! {
        biased;
        _ = rpc_stop_requested(stop) => ReceiptPhase::Stopping,
        got = r.wait(SESSION_REPLY_TIMEOUT) => match got {
            Some(got) => ReceiptPhase::Reply(map_receipt(got)),
            None => ReceiptPhase::Timeout,
        },
    }
}

fn stopped_before_accept_error() -> RpcError {
    RpcError::new(
        ErrorCode::SessionBusy,
        "this daemon is stopping and the session had not taken the instruction",
    )
    .with_hint("nothing happened — the instruction was withdrawn before shutdown")
}

fn taken_during_stop_error() -> RpcError {
    RpcError::new(
        ErrorCode::Internal,
        "this daemon is stopping after the session took the instruction",
    )
    .with_hint("it may still take effect — check the session state before retrying")
}

async fn reply_within_state<T>(
    r: &mut crate::rc::ticket::Receipt<T>,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) -> ReplyWait<T> {
    // First leg: wait for it to be **taken**.
    match receipt_phase(r, stop).await {
        ReceiptPhase::Reply(result) => return ReplyWait::Done(result),
        ReceiptPhase::Stopping => {
            return match r.abandon() {
                crate::rc::ticket::Abandon::NeverRan => {
                    ReplyWait::Done(Err(stopped_before_accept_error()))
                }
                crate::rc::ticket::Abandon::AlreadyTaken => {
                    ReplyWait::InFlight(taken_during_stop_error())
                }
            };
        }
        ReceiptPhase::Timeout => {}
    }

    // Timed out. Withdraw first — **only a withdrawal that succeeds earns the right to say
    // "nothing happened"**.
    match r.abandon() {
        crate::rc::ticket::Abandon::NeverRan => ReplyWait::Done(Err(RpcError::new(
            ErrorCode::SessionBusy,
            "that session is finishing a turn and did not pick this up in time",
        )
        .with_hint(
            "nothing happened — the instruction was withdrawn before the session took it, so it is safe to try again",
        ))),
        // Cannot withdraw: it has been taken, its side effects are on their way, and withdrawal
        // no longer applies. Wait one more leg; if that also runs out, **say so plainly** — an
        // answer of "safe to retry" makes the caller redo a mode change that already started.
        crate::rc::ticket::Abandon::AlreadyTaken => match receipt_phase(r, stop).await {
            ReceiptPhase::Reply(result) => ReplyWait::Done(result),
            ReceiptPhase::Stopping => ReplyWait::InFlight(taken_during_stop_error()),
            ReceiptPhase::Timeout => ReplyWait::InFlight(RpcError::new(
                ErrorCode::Internal,
                "that session took the instruction but has not answered",
            )
            .with_hint(
                "it may still take effect — check the session's current state before retrying",
            )),
        },
    }
}

#[cfg(test)]
async fn reply_within<T>(r: &mut crate::rc::ticket::Receipt<T>) -> Result<T, RpcError> {
    let (_stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    match reply_within_state(r, &mut stop_rx).await {
        ReplyWait::Done(result) => result,
        ReplyWait::InFlight(error) => Err(error),
    }
}

/// Who this frame really speaks for — **and** that the workspace named in params is that one.
///
/// Done once before dispatch, so every arm afterwards can read `p.workspace_id` safely.
///
/// # Why this funnels into one place
///
/// `params` comes entirely from the browser. `session.list` / `session.watch` / `fs.readFile` /
/// `terminal.open` / `project.bind` / `session.start` / `session.resume` / `session.unwatch` each
/// read `p.workspace_id` and act on it, and with none of them comparing it against the one the
/// hub stamped, a member of A can point these verbs at B on the same machine. Missing all of them
/// at once is not a coincidence: putting the test inside the arms means every new verb needs
/// someone to **remember** to add a line, and missing one has no symptom at all.
///
/// An unstamped frame is always refused. A missing claim must not be read as a wildcard —
/// `caller == None` skipping the ownership check is exactly that.
fn caller_scope(f: &Frame) -> Result<crate::protocol::CallerClaim, RpcError> {
    let Some(caller) = f.caller.as_ref() else {
        return Err(RpcError::new(
            ErrorCode::Unauthenticated,
            "this frame carries no caller claim, so this machine cannot tell which workspace is asking",
        )
        .with_hint("every frame the hub relays is stamped; one without a stamp is a hub bug"));
    };
    if let Some(named) = f.params_workspace_id()
        && named != caller.workspace_id
    {
        // The same answer as "this machine has no such workspace": confirming that id exists
        // here is itself a leak.
        return Err(RpcError::new(
            ErrorCode::WorkspaceNotFound,
            format!("workspace {named} is not bound on this machine"),
        )
        .with_hint("a workspace can only be driven from its own page"));
    }
    Ok(caller.clone())
}

/// A viewer's identity, used for watch bookkeeping.
///
/// With no `account_id` (an old hub sends none) this falls back to the role — several people in
/// one role then collapse into one entry, and the cost is that the tail stops only when the last
/// of them leaves, rather than one person stopping it for another. That direction is right.
fn caller_key(caller: &crate::protocol::CallerClaim) -> String {
    caller
        .account_id
        .clone()
        .unwrap_or_else(|| format!("role:{}", caller.role))
}

/// "That session does not exist" — "not here" and "not yours" must answer with the same
/// sentence: confirming that an id runs on this machine is itself a leak.
fn no_such_session(session_id: &str) -> RpcError {
    RpcError::new(
        ErrorCode::SessionNotFound,
        format!("no live session {session_id}"),
    )
    .with_hint(
        "check this workspace's session list — it may have ended, or it may never have been here",
    )
}

/// The id of a read-only watch stream. **Workspace-qualified**, not just the harness's session
/// id.
///
/// Keying on `agit-watch-{session_id}` alone says "several people watching one session share one
/// tail and one numbering". That holds for several viewers **inside one workspace** and fails
/// across workspaces: one directory can be bound by two workspaces, so A and B watching the same
/// local session collide on one key. Three consequences follow — one `session.unwatch` from A
/// decrements the viewer count on B's tail, and at zero the whole thing is cut off (the other
/// side sees the live view stop with no warning); the hub's
/// `remember_workspace(stream, workspace)` is last-write-wins, so this stream's events fan out to
/// whichever workspace subscribed later; and once unwatch checks ownership, anyone outside the
/// workspace that watched first can **never** decrement their own count, `viewers` never reaches
/// 0, and the polling stays on the machine forever.
///
/// The cost is two tails when two workspaces watch the same transcript at once. A reader breaks
/// nothing; the cost is one redundant poll, with `WATCH_IDLE_STOP` as the backstop.
fn watch_stream_id(workspace_id: &str, session_id: &str) -> String {
    format!("agit-watch-{workspace_id}-{session_id}")
}

/// Whether this frame may **read** a given session.
fn require_same_workspace(
    caller: &crate::protocol::CallerClaim,
    session_id: &str,
    session_workspace: &str,
) -> Result<(), RpcError> {
    // One machine serves several workspaces at once, and the session id comes entirely from the
    // browser. The role is real, it is just being spent on another tenant — overlapping paths are
    // not a tenant boundary, so what is compared is the workspace id.
    if caller.workspace_id != session_workspace {
        return Err(no_such_session(session_id));
    }
    Ok(())
}

/// Whether a session verb **adds** to what this session can do, or **subtracts**.
///
/// The split is not there to save a check; the two classes have opposite right answers. Adding
/// consults the danger bit, subtracting must not — "you need permission to become safer" is
/// absurd. Interrupt is a brake, not a steering wheel: blocking it means a runaway bypass session
/// can only be stopped by its owner, and the owner may be asleep.
#[derive(Clone, Copy, PartialEq)]
enum Need {
    /// Interrupt, refuse an approval, tighten the guard. Needs only ownership plus the right to
    /// issue instructions.
    Brake,
    /// Send a message, steer, allow an approval. Also passes the danger gate.
    Drive,
}

/// The hub's `agent` / `branch` → one **validated** lineage.
///
/// Half of it counts as none: half a lineage is worse than none — `agit commit --from-hook`
/// settles with a string it cannot parse a branch out of, and the error it reports looks nothing
/// like "never set at all".
///
/// **Supplied but invalid rejects the whole request**, rather than degrading to "no lineage".
/// The values on this path come off the wire, and an owner that cannot be turned into a path has
/// only two sources: a hub bug, or someone probing for path escape. Neither may let the session
/// run as usual — it would run all the way into the Stop hook and build
/// `~/.agit/repos/<owner>/<name>` out of that same string, and by then it is another process.
///
/// (The path that reads back from the roster does not come through here: that is our own old
/// ledger, see `resume_session`.)
fn lineage_from_params(
    agent_identity_acked: bool,
    agent: Option<&str>,
    expected_agent_id: Option<&str>,
    branch: Option<&str>,
) -> Result<Option<crate::rc::lineage::AgitSession>, RpcError> {
    // An old hub may still send slug-only lineage. Without an explicit feature
    // ACK it is display data, never authority to mutate a repository.
    if !agent_identity_acked {
        return Ok(None);
    }
    if agent.is_none() && expected_agent_id.is_none() && branch.is_none() {
        return Ok(None);
    }
    let (Some(a), Some(id), Some(b)) = (agent, expected_agent_id, branch) else {
        return Err(RpcError::new(
            ErrorCode::MalformedFrame,
            "agent_identity_v1 lineage must include agent, expected_agent_id, and branch",
        )
        .with_hint("upgrade the hub; partial repository lineage is never settled"));
    };
    if a.is_empty() || id.is_empty() || b.is_empty() {
        return Err(RpcError::new(
            ErrorCode::MalformedFrame,
            "agent_identity_v1 lineage fields must not be empty",
        )
        .with_hint("the hub must omit all lineage fields or send all three"));
    }
    crate::rc::lineage::AgitSession::new(a, id, b)
        .map(Some)
        .map_err(|e| {
            RpcError::new(ErrorCode::PathNotAllowed, e.to_string())
                .with_hint("the hub sent a lineage this machine cannot turn into a repository path")
        })
}

/// Interpret a `session.start` key under the current socket's negotiated
/// feature set. Presence alone is not authority: an un-ACKed peer must stay on
/// the legacy path, while an ACKed peer must never fall back to an unkeyed
/// launch.
fn negotiated_start_id(
    feature_acked: bool,
    start_id: Option<&str>,
) -> Result<Option<String>, RpcError> {
    match (feature_acked, start_id) {
        (false, None) => Ok(None),
        (false, Some(_)) => Err(RpcError::new(
            ErrorCode::MalformedFrame,
            "session.start supplied start_id without negotiating session_start_idempotency_v1",
        )
        .with_hint("re-register this socket and wait for the feature ACK before starting")),
        (true, None) => Err(RpcError::new(
            ErrorCode::MalformedFrame,
            "session.start requires start_id after session_start_idempotency_v1 is ACKed",
        )
        .with_hint("retry with the same UUID generated for this start intent")),
        (true, Some(raw)) => uuid::Uuid::parse_str(raw.trim())
            .map(|id| Some(id.to_string()))
            .map_err(|_| {
                RpcError::new(
                    ErrorCode::MalformedFrame,
                    "session.start start_id must be a UUID",
                )
                .with_hint("generate one UUID and reuse it for every retry of this start")
            }),
    }
}

fn pending_start_error(start_id: &str, session_id: &str) -> RpcError {
    RpcError::new(
        ErrorCode::SessionBusy,
        format!(
            "session.start {start_id} is pending with logical session {session_id}; its launch outcome is intentionally not replayed"
        ),
    )
    .with_hint(
        "inspect session.list and this machine; do not choose a new start_id because that could launch a duplicate agent",
    )
}

fn conflicting_start_error() -> RpcError {
    RpcError::new(
        ErrorCode::MalformedFrame,
        "session.start reused start_id with different immutable launch parameters",
    )
    .with_hint("reuse a start_id only when the same immutable launch inputs are retried")
}

fn lost_start_history_error() -> RpcError {
    RpcError::new(
        ErrorCode::SessionBusy,
        "session.start idempotency history is unavailable on this machine",
    )
    .with_hint(
        "restore ~/.agit/rc/sessions.json before starting; choosing a new start_id could launch a duplicate agent",
    )
}

fn roster_lineage(
    agent_identity_acked: bool,
    entry: &roster::Entry,
) -> Option<crate::rc::lineage::AgitSession> {
    if !agent_identity_acked {
        return None;
    }
    entry
        .agit_session
        .as_deref()
        .zip(entry.expected_agent_id.as_deref())
        .and_then(|(lineage, expected_agent_id)| {
            crate::rc::lineage::AgitSession::parse(lineage, expected_agent_id).ok()
        })
}

/// Resume may add lineage only to a row that has never claimed any repository
/// identity. A slug-only legacy row is different: it may refer to a deleted
/// object whose name was reused, so the current hub cannot fill in its missing
/// ID. The later `rc land` pin check independently refuses a legacy checkout.
fn resume_lineage(
    agent_identity_acked: bool,
    entry: &roster::Entry,
    agent: Option<&str>,
    expected_agent_id: Option<&str>,
    branch: Option<&str>,
) -> Result<Option<crate::rc::lineage::AgitSession>, RpcError> {
    if entry.agit_session.is_some() || entry.expected_agent_id.is_some() {
        Ok(roster_lineage(agent_identity_acked, entry))
    } else {
        lineage_from_params(agent_identity_acked, agent, expected_agent_id, branch)
    }
}

/// Whether this session's transcript is still growing (= likely open in another terminal).
fn transcript_recently_written(runtime: &str, thread_id: &str, cwd: &std::path::Path) -> bool {
    let Ok(adapter) = crate::adapter::get(runtime) else {
        return false;
    };
    let Some(p) = adapter.resolve(thread_id, Some(cwd)) else {
        return false;
    };
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .map(recently_written)
        .unwrap_or(false)
}

/// The transcript's total line count, and the byte offset the "last `want` lines" start at.
///
/// # Why one scan plus a ring buffer
///
/// watch replays only the tail, but has to report **absolute** line numbers
/// (`item.completed.line` is the physical line number in the file, the same coordinate
/// `agit show` uses; making it relative makes it a different thing). An absolute line number
/// needs the total line count, and the total line count needs reading the whole file.
///
/// So scan once, and remember where the recent lines start in a ring buffer holding only `want`
/// offsets. Time O(file), memory O(want). `Tailer::new(.., from_start)` cannot do this: it reads
/// **every line** into a `Vec` and then drops the front, taking as much memory as the whole
/// transcript.
fn tail_window(path: &std::path::Path, want: u64) -> (u64, u64, u64, bool) {
    use std::io::{BufRead as _, BufReader, Seek as _, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return (0, 0, 0, true);
    };
    let want = want.max(1) as usize;

    // **Never scan more bytes than this.**
    //
    // An exact line number counts from the head of the file, and this function runs synchronously
    // inside `dispatch` while holding the daemon's global mutex: the cost grows linearly with the
    // size of the transcript and has no bound of its own — one pathologically large transcript
    // stalls every RPC on this machine, event pump included.
    //
    // So cap it. Over the cap only the last stretch is scanned: line numbers become **relative to
    // the truncation point**, and their only uses are numbering this watch stream's items (a
    // stream is created fresh for each watch, so the numbering only has to be unique and
    // monotonic within it) and the "showing the last N lines" line in the interface — neither
    // depends on absolute line numbers. Below `SCAN_CAP` everything stays exact, and that covers
    // the vast majority of transcripts.
    const SCAN_CAP: u64 = 32 * 1024 * 1024;
    let mut base_off: u64 = 0;
    if let Ok(meta) = f.metadata()
        && meta.len() > SCAN_CAP
    {
        base_off = meta.len() - SCAN_CAP;
        if f.seek(SeekFrom::Start(base_off)).is_ok() {
            // Align to the next newline; never start in the middle of a line.
            let mut skip = Vec::with_capacity(8192);
            let mut r = BufReader::new(&mut f);
            if let Ok(n) = r.read_until(b'\n', &mut skip) {
                base_off += n as u64;
            }
            let _ = f.seek(SeekFrom::Start(base_off));
        } else {
            base_off = 0;
        }
    }
    let mut starts: std::collections::VecDeque<(u64, u64)> = std::collections::VecDeque::new();
    let mut reader = BufReader::new(f);
    let mut offset: u64 = base_off;
    let mut lineno: u64 = 0;
    let mut buf = Vec::with_capacity(8192);
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if starts.len() == want {
                    starts.pop_front();
                }
                starts.push_back((offset, lineno));
                offset += n as u64;
                lineno += 1;
            }
        }
    }
    // **An empty window must not fall back to the head of the file.**
    //
    // After capping, the offset aligns to the next newline; if the last JSONL record runs from
    // the truncation point all the way to EOF, that alignment pushes `base_off` to EOF and
    // `starts` is empty — and `(0, 0)` makes the tailer re-read from the **head of the file**,
    // going straight around the `SCAN_CAP` guardrail and pulling the whole transcript into
    // memory. The right answer for an empty window is "there is nothing after this point", not
    // "start over from the beginning".
    let (start_off, start_line) = starts.front().copied().unwrap_or((base_off, 0));
    // The fourth value: whether the two line numbers above are **physical** ones. After capping
    // they are relative to the truncation point, while every other line number in the protocol is
    // physical — that has to be said out loud, see `SessionWatchResult`.
    (start_off, start_line, lineno, base_off == 0)
}

/// Fill "at most `want` bytes" for real, instead of taking whatever one `read()` hands back.
///
/// A single `read()` legitimately returns less: a signal interrupt, a network filesystem and a
/// pipe-like special file all return a fragment. If the preview stops after one `read`, a short
/// read is silently taken for end of file: the fixed-cap window the user sees is missing its
/// second half, and `truncated` is wrong along with it. An error must not masquerade as a
/// successful empty preview either.
fn read_up_to(reader: &mut impl std::io::Read, want: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut buf = Vec::with_capacity(want);
    reader.take(want as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

fn is_utf8_continuation(byte: u8) -> bool {
    byte & 0b1100_0000 == 0b1000_0000
}

/// Assign every UTF-8 scalar to the preview chunk containing its **leading**
/// byte. A caller can therefore page with `offset += FILE_PREVIEW_CAP` without
/// either losing a split character or rendering it twice.
///
/// `bytes` starts exactly at the requested offset and carries up to three
/// look-ahead bytes. Continuation bytes at the front belong to the preceding
/// chunk and are skipped; continuation bytes after `nominal_len` complete a
/// scalar whose leading byte belongs to this chunk. The returned text window
/// may consequently exceed the nominal byte cap by at most three bytes.
fn utf8_preview_window(bytes: &[u8], nominal_len: usize) -> &[u8] {
    let nominal_len = nominal_len.min(bytes.len());
    let mut start = 0;
    while start < bytes.len() && is_utf8_continuation(bytes[start]) {
        start += 1;
    }
    if start >= nominal_len {
        return &bytes[start..start];
    }
    let mut end = nominal_len;
    while end < bytes.len() && is_utf8_continuation(bytes[end]) {
        end += 1;
    }
    &bytes[start..end]
}

/// Read one file for the previewer.
///
/// Text comes back as a string, binary as base64 — the previewer wants something it can drop
/// straight into `<img src>` / `<embed>` / `<audio>`. Past the cap it truncates rather than
/// refusing: a log too big to fit should still show its beginning.
fn read_preview(path: &std::path::Path, offset: u64) -> Result<FsReadFileResult, RpcError> {
    use std::io::{Seek, SeekFrom};
    let meta = std::fs::metadata(path).map_err(|e| {
        RpcError::new(
            ErrorCode::SessionNotFound,
            format!("cannot stat {}: {e}", path.display()),
        )
    })?;
    if meta.is_dir() {
        return Err(
            RpcError::new(ErrorCode::MalformedFrame, "that path is a directory")
                .with_hint("use fs.readDirectory for folders"),
        );
    }
    let size = meta.len();
    let mut f = std::fs::File::open(path).map_err(|e| {
        RpcError::new(
            ErrorCode::PathNotAllowed,
            format!("cannot open {}: {e}", path.display()),
        )
    })?;
    if offset > 0 {
        f.seek(SeekFrom::Start(offset)).map_err(|e| {
            RpcError::new(
                ErrorCode::PathNotAllowed,
                format!("cannot seek {} to byte {offset}: {e}", path.display()),
            )
        })?;
    }
    let remaining = size.saturating_sub(offset);
    let nominal_len = FILE_PREVIEW_CAP.min(remaining) as usize;
    // UTF-8 scalars are at most four bytes. Three look-ahead bytes suffice to
    // finish a scalar whose leading byte is the last byte of the nominal
    // window; binary responses still encode only `nominal_len` bytes.
    let read_len = (FILE_PREVIEW_CAP + 3).min(remaining) as usize;
    let buf = read_up_to(&mut f, read_len).map_err(|e| {
        RpcError::new(
            ErrorCode::PathNotAllowed,
            format!("cannot read {} at byte {offset}: {e}", path.display()),
        )
    })?;
    let nominal = &buf[..nominal_len.min(buf.len())];

    let mime = mime_of(path);
    // The test is "does it contain a NUL byte", not the extension: an extension lies, and no
    // text format should contain a NUL.
    let is_binary = nominal.contains(&0) || !mime.starts_with("text/") && !is_texty(&mime);
    // Pagination advances by the nominal byte window, not by the UTF-8
    // look-ahead included in this response.
    let truncated = offset + (nominal_len as u64) < size;

    Ok(if is_binary {
        use base64::Engine as _;
        FsReadFileResult {
            path: path.to_string_lossy().to_string(),
            size,
            mime,
            text: None,
            base64: Some(base64::engine::general_purpose::STANDARD.encode(nominal)),
            is_binary: true,
            truncated,
        }
    } else {
        FsReadFileResult {
            path: path.to_string_lossy().to_string(),
            size,
            mime,
            text: Some(String::from_utf8_lossy(utf8_preview_window(&buf, nominal_len)).to_string()),
            base64: None,
            is_binary: false,
            truncated,
        }
    })
}

/// Extension → MIME. Covers only what the previewer actually renders; everything else is
/// octet-stream.
fn mime_of(path: &std::path::Path) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "pdf" => "application/pdf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "mp4" => "video/mp4",
        "json" => "application/json",
        "md" => "text/markdown",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" | "ts" | "tsx" | "jsx" => "text/javascript",
        "rs" | "py" | "go" | "java" | "c" | "h" | "cpp" | "sh" | "toml" | "yaml" | "yml"
        | "sql" | "txt" | "log" | "ini" | "cfg" | "lock" | "" => "text/plain",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// These MIME types do not start with `text/`, but their content is text.
fn is_texty(mime: &str) -> bool {
    matches!(
        mime,
        "application/json" | "image/svg+xml" | "text/javascript"
    )
}

fn finish_local_sessions(
    mut sessions: Vec<LocalSession>,
    purpose: LocalSessionScan,
    mut gist_for: impl FnMut(&LocalSession) -> Option<String>,
) -> Vec<LocalSession> {
    // Most recently talked to first — "continue where I left off" almost always means the most
    // recent one.
    sessions.sort_by(|a, b| b.modified_at.cmp(&a.modified_at));
    sessions.dedup_by(|a, b| a.runtime_session_id == b.runtime_session_id);

    if purpose == LocalSessionScan::Listing {
        // Spend parsing only on the leading few that **have no gist yet**. Nothing from codex
        // reaches here.
        let mut budget = LOCAL_GIST_BUDGET;
        for item in &mut sessions {
            if budget == 0 {
                break;
            }
            if item.gist.is_some() {
                continue;
            }
            budget -= 1;
            item.gist = gist_for(item);
        }
    }
    sessions
}

/// Resolve one already-enumerated local session.
///
/// `scan` is `FnOnce` on purpose: resume needs both `likely_active` and launch coordinates, and
/// they must come from the same snapshot. The old code called the full scanner once for each.
fn locate_local_with(
    mirror: &Mirror,
    workspace_id: &str,
    runtime_session_id: &str,
    scan: impl FnOnce() -> Vec<LocalSession>,
) -> Result<LocatedLocal, RpcError> {
    let cand = scan()
        .into_iter()
        .find(|candidate| candidate.runtime_session_id == runtime_session_id)
        .ok_or_else(|| {
            RpcError::new(
                ErrorCode::SessionNotFound,
                format!("no local session {runtime_session_id} under this workspace's folders"),
            )
            .with_hint(
                "refresh the session list; it may have been in a folder that is no longer bound",
            )
        })?;
    let cwd = PathBuf::from(&cand.cwd);
    let project_id = mirror.workspaces.get(workspace_id).and_then(|projects| {
        projects
            .iter()
            .find(|(_, path)| cwd.starts_with(std::path::Path::new(path)))
            .map(|(id, _)| id.clone())
    });
    Ok(LocatedLocal {
        runtime: cand.runtime,
        cwd,
        project_id,
        likely_active: cand.likely_active,
    })
}

/// Whether the transcript file was written to just now (= likely open in someone else's
/// terminal).
fn recently_written(mtime: std::time::SystemTime) -> bool {
    mtime
        .elapsed()
        .map(|d| (d.as_secs() as i64) < LIKELY_ACTIVE_SECS)
        .unwrap_or(false)
}

/// `SystemTime` → RFC3339. The local session list sorts on it ("most recently talked to" first).
fn rfc3339(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or_default()
        .to_rfc3339()
}

#[cfg(test)]
mod danger_gate_tests;

#[cfg(test)]
mod tail_window_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod preview_tests;
