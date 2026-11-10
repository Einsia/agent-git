//! Harness drivers: how `agitd` speaks to a live coding agent.
//!
//! # Two shapes, deliberately not flattened
//!
//! claude-code and codex both have complete control planes, but they are not the
//! same shape. Flattening them to a lowest common denominator would cost both
//! sides their best feature (codex's protocol-level steer; claude-code's rich
//! `can_use_tool` payload). So each driver reports a [`crate::protocol::RuntimeCapability`]
//! at register time and the frontend renders from that.
//!
//! | | codex | claude-code |
//! |---|---|---|
//! | transport | app-server, JSON-RPC over stdio | `-p --input-format stream-json --output-format stream-json` |
//! | start / resume | `thread/start`, `thread/resume` | `--session-id`, `--resume`, `--fork-session` |
//! | mid-turn steer | `turn/steer` (first-class verb, immediate) | another user message on stdin (lands at the tool boundary) |
//! | interrupt | `turn/interrupt` | `control_request` `{"subtype":"interrupt"}` |
//! | approvals | `item/*/requestApproval` server→client requests | `can_use_tool` control request (needs `--permission-prompt-tool stdio`) |
//! | deltas | `item/agentMessage/delta` etc. | `--include-partial-messages` |
//! | echo own messages | in protocol | `--replay-user-messages` |
//!
//! All of the above was verified against the installed binaries, not read off a
//! doc page — see `docs/rc-harness-probe.md`.
//!
//! # What a driver is responsible for
//!
//! Only the live control plane: start the process, push messages in, pull
//! structured events out, answer approvals, interrupt. It does **not** produce
//! the permanent record — that comes from tailing the harness's own transcript
//! file (see [`super::tail`]), because that file is exactly what `agit commit`
//! snapshots. Keeping those two jobs apart is what guarantees the live view and
//! the committed history can never disagree.

pub mod claude_code;
#[cfg(unix)]
pub mod claude_native;
pub mod codex;
pub mod models;
pub mod opencode;
pub mod proc;

/// Proves that steering was refused before any input reached the native process.
#[derive(Debug)]
pub(crate) struct TurnNotRunning;

impl std::fmt::Display for TurnNotRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no turn is running — send a message instead of steering")
    }
}

impl std::error::Error for TurnNotRunning {}

/// A turn-fenced interrupt on a runtime that cannot confirm which turn is running. Nothing was
/// sent.
#[derive(Debug)]
pub(crate) struct FenceUnsupported;

impl std::fmt::Display for FenceUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The stop was not sent: this conversation can't confirm which turn is running.")
    }
}

impl std::error::Error for FenceUnsupported {}

/// An attach-only opening found no terminal to join when its driver was chosen (it may have
/// exited after the opening was admitted). Nothing was launched.
#[derive(Debug)]
pub(crate) struct AttachOnlyNotRunning;

impl std::fmt::Display for AttachOnlyNotRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("this conversation is not running on this device")
    }
}

impl std::error::Error for AttachOnlyNotRunning {}

use crate::protocol::{
    ApprovalRequest, ApprovalResponse, Delivery, ItemKind, PermissionApply, PermissionMode,
    RuntimeCapability, SlashCommand,
};
use serde_json::Value;
use std::collections::HashSet;
use std::path::PathBuf;

/// Marks a runtime-owned Stop hook whose settlement is delegated to the
/// daemon's dynamically cancellable supervisor path.
pub const SUPERVISED_HOOK_ENV: &str = "AGIT_RC_SUPERVISED_HOOK";

/// Human-facing explanation shared by the two repository write entry points.
/// The harness environment is a launch-time snapshot; only the supervisor has
/// the live negotiated-feature lease needed to authorize a settlement write.
pub const SUPERVISED_SETTLEMENT_MESSAGE: &str =
    "repository settlement in this RC session is owned by the RC supervisor";

/// Native turn ids are untrusted protocol input. Keeping lifetime tombstones
/// is required for delayed-frame safety, so bound both each id and the whole
/// generation instead of silently evicting identities that may still arrive.
pub(crate) const MAX_NATIVE_TURN_ID_BYTES: usize = 256;
pub(crate) const MAX_TURN_IDS_PER_GENERATION: usize = 65_536;
pub(crate) const MAX_TURN_ID_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NativeTurnIdentityError {
    Empty,
    TooLong { bytes: usize, max: usize },
    EntryLimit { max: usize },
    PayloadLimit { bytes: usize, max: usize },
    PayloadOverflow,
}

impl std::fmt::Display for NativeTurnIdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("native turn id is empty"),
            Self::TooLong { bytes, max } => {
                write!(f, "native turn id is {bytes} bytes; maximum is {max}")
            }
            Self::EntryLimit { max } => {
                write!(f, "native turn identity budget reached {max} entries")
            }
            Self::PayloadLimit { bytes, max } => write!(
                f,
                "native turn identity payload would reach {bytes} bytes; maximum is {max}"
            ),
            Self::PayloadOverflow => f.write_str("native turn identity payload size overflowed"),
        }
    }
}

pub(crate) fn validate_native_turn_id(turn_id: &str) -> Result<(), NativeTurnIdentityError> {
    if turn_id.is_empty() {
        return Err(NativeTurnIdentityError::Empty);
    }
    if turn_id.len() > MAX_NATIVE_TURN_ID_BYTES {
        return Err(NativeTurnIdentityError::TooLong {
            bytes: turn_id.len(),
            max: MAX_NATIVE_TURN_ID_BYTES,
        });
    }
    Ok(())
}

/// Harness-generation identity tombstones with explicit fail-stop limits.
///
/// A duplicate remains harmless even when the set is full; only a genuinely
/// new identity consumes budget. A new harness generation starts with a fresh
/// set after the old native stdout producer has been terminated.
#[derive(Debug)]
pub(crate) struct BoundedTurnIds {
    ids: HashSet<String>,
    payload_bytes: usize,
    max_entries: usize,
    max_payload_bytes: usize,
}

impl Default for BoundedTurnIds {
    fn default() -> Self {
        Self {
            ids: HashSet::new(),
            payload_bytes: 0,
            max_entries: MAX_TURN_IDS_PER_GENERATION,
            max_payload_bytes: MAX_TURN_ID_PAYLOAD_BYTES,
        }
    }
}

impl BoundedTurnIds {
    pub(crate) fn contains(&self, turn_id: &str) -> bool {
        self.ids.contains(turn_id)
    }

    pub(crate) fn try_insert(&mut self, turn_id: String) -> Result<bool, NativeTurnIdentityError> {
        validate_native_turn_id(&turn_id)?;
        if self.ids.contains(&turn_id) {
            return Ok(false);
        }
        if self.ids.len() >= self.max_entries {
            return Err(NativeTurnIdentityError::EntryLimit {
                max: self.max_entries,
            });
        }
        let payload_bytes = self
            .payload_bytes
            .checked_add(turn_id.len())
            .ok_or(NativeTurnIdentityError::PayloadOverflow)?;
        if payload_bytes > self.max_payload_bytes {
            return Err(NativeTurnIdentityError::PayloadLimit {
                bytes: payload_bytes,
                max: self.max_payload_bytes,
            });
        }
        let inserted = self.ids.insert(turn_id);
        debug_assert!(inserted, "a duplicate was checked before insertion");
        self.payload_bytes = payload_bytes;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.ids.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn with_limits(max_entries: usize, max_payload_bytes: usize) -> Self {
        Self {
            ids: HashSet::new(),
            payload_bytes: 0,
            max_entries,
            max_payload_bytes,
        }
    }

    #[cfg(test)]
    pub(crate) fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }
}

pub fn settlement_is_delegated() -> bool {
    std::env::var(SUPERVISED_HOOK_ENV).as_deref() == Ok("1")
}

pub(crate) fn is_request_id_exhaustion(error: &anyhow::Error) -> bool {
    error.downcast_ref::<codex::RequestIdExhausted>().is_some()
}

fn lineage_env(lineage: Option<&crate::rc::lineage::AgitSession>) -> Vec<(String, String)> {
    let mut env = vec![(SUPERVISED_HOOK_ENV.into(), "1".into())];
    if let Some(lineage) = lineage {
        env.push(("AGIT_SESSION".into(), lineage.to_string()));
        if let Some(kind) = &lineage.capture {
            env.push((
                crate::rc::capture::CAPTURE_ENV.into(),
                serde_json::to_string(kind).expect("capture kind serializes"),
            ));
            env.push(("AGIT_LOCAL_AGENT_ID".into(), lineage.agent_id().into()));
            return env;
        }
        env.push((
            crate::hub::identity::EXPECTED_AGENT_ID_ENV.into(),
            lineage.agent_id().into(),
        ));
    }
    env
}

/// How a session should be launched.
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// Working directory. Already checked against the allowlist by the caller.
    pub cwd: PathBuf,
    /// Resume this harness-native session id, or `None` to start fresh.
    pub resume_from: Option<String>,
    /// `AGIT_SESSION` + `AGIT_EXPECTED_AGENT_ID`, injected together so every
    /// settlement consumer has both the route and immutable repository identity.
    ///
    /// **The parsed form.** It is injected into the child environment right
    /// here, and the child's Stop hook uses it to build
    /// `~/.agit/repos/<owner>/<name>` — another process, before landing, so no
    /// "validate at landing" arrangement on this side can catch it. The test
    /// belongs where the value is born, see [`crate::rc::lineage`].
    pub agit_session: Option<crate::rc::lineage::AgitSession>,
    /// Model override, if the caller asked for one.
    pub model: Option<String>,
    /// The session was requested with permission checks disabled. Recorded so
    /// the hub can lock the workspace to its owner.
    pub dangerous: bool,
    /// How much the agent may do unattended. `None` = [`PermissionMode::Default`].
    pub permission_mode: Option<PermissionMode>,
    /// Join a conversation that already runs in a terminal, never launch one: when no terminal
    /// can be attached at driver selection, the launch is refused with `AttachOnlyNotRunning`.
    pub attach_only: bool,
    /// A project the owner shared with a group (`rc::project_share`), recorded on its roster row.
    pub project_share: bool,
}

impl LaunchSpec {
    /// The mode to launch under, resolving both spellings of "no checks".
    ///
    /// `dangerous` predates [`PermissionMode`] and callers still set it, so the
    /// two must not be able to disagree: whichever says "bypass" wins.
    pub fn effective_mode(&self) -> PermissionMode {
        let asked = self.permission_mode.unwrap_or(PermissionMode::Default);
        if self.dangerous && !asked.is_dangerous() {
            return PermissionMode::Bypass;
        }
        asked
    }
}

/// What a driver emits upward. Deliberately smaller than the protocol's event
/// set: the supervisor is what turns these into numbered frames, merges them
/// with tailed transcript lines, and applies redaction.
#[derive(Debug, Clone)]
pub enum HarnessEvent {
    /// The harness told us its native session/thread id. Arrives once, early.
    /// The supervisor keeps this in its private map and never puts it on the
    /// wire — viewers address sessions by the logical agit id only.
    Ready {
        runtime_thread_id: String,
        transcript_path: Option<PathBuf>,
    },
    TurnStarted {
        turn_id: String,
        prompt: Option<String>,
    },
    ItemStarted {
        item_id: String,
        kind: ItemKind,
        tool: Option<String>,
    },
    /// Token-level text. Cheap and lossy by design: never persisted, always
    /// superseded by the transcript line.
    Delta {
        item_id: String,
        text: String,
    },
    ItemCompleted {
        item_id: String,
    },
    TurnCompleted {
        turn_id: String,
        outcome: TurnOutcome,
        error: Option<String>,
        cost_usd: Option<f64>,
        duration_ms: Option<u64>,
    },
    /// Resolution of a client-originated turn start.
    ///
    /// Codex answers `turn/start` on the same stdout stream as notifications
    /// and server requests. Keeping the typed outcome in that stream lets the
    /// supervisor process everything that preceded the response before it
    /// closes the viewer RPC receipt.
    TurnStartResolved(TurnStartOutcome),
    SteerResolved(Result<Delivery, String>),
    /// The plugin handed a message it held for the running turn to the model with a tool step's
    /// result.
    StepSteerDelivered {
        operation_id: String,
    },
    /// A message held for a turn's next tool step that the turn ended without: `interrupted` when
    /// the turn was stopped.
    StepSteerUndelivered {
        operation_id: String,
        interrupted: bool,
    },
    InterruptResolved(Result<InterruptOutcome, String>),
    /// An exact response arrived after a notification-only acceptance was
    /// already returned. The daemon may clear its durable Plan restart
    /// override only when this opaque token matches the outstanding one.
    TurnStartConfirmed {
        confirmation_token: Option<String>,
        effective_mode: Option<PermissionMode>,
    },
    Approval(ApprovalRequest),
    /// The native request is no longer answerable, including through another client.
    ApprovalResolved {
        approval_id: String,
    },
    /// Native defaults changed, potentially through another shared client.
    SettingsUpdated {
        mode: Option<PermissionMode>,
        applied: PermissionApply,
    },
    /// Model metadata changed without changing the native permission authority.
    ModelUpdated,
    GoalUpdated {
        goal: Value,
    },
    /// The harness process exited.
    Exited {
        code: Option<i32>,
    },
    /// Native frames contradicted an already-authoritative lifecycle fact.
    /// Continuing could resurrect a completed turn or misattribute input, so
    /// the supervisor must terminate the harness tree.
    ProtocolInvariant {
        message: String,
        /// Sticky mode carried by a still-unresolved native turn/start, if
        /// this contradiction interrupted one. The supervisor must deliver it
        /// through the typed Unknown receipt before releasing the RPC gate.
        attempted_mode: Option<PermissionMode>,
        /// Token whose notification-only acceptance was already ACKed behind
        /// a durable Plan restart override. A contradiction may not publish
        /// Ended until the daemon confirms that override remains safe.
        confirmation_token: Option<String>,
    },
    /// Something we could not parse or a stderr line worth surfacing.
    Notice {
        text: String,
    },
    /// A runtime status composed from protocol fields, without stderr content.
    Progress {
        text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Ok,
    Interrupted,
    Error,
}

/// The authorization-relevant result of asking a harness to start one turn.
///
/// This is deliberately not an `anyhow::Result`: an explicit native refusal
/// proves that no sticky permission override landed and is safe to retry,
/// while an unknown outcome may already have consumed the prompt and mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStartOutcome {
    NativeSubmission {
        operation_id: String,
        delivery: Delivery,
    },
    Accepted {
        turn_id: String,
        /// False when native `turn/completed` preceded this exact response.
        /// The response still proves acceptance, but must not resurrect the
        /// finished turn as live.
        still_running: bool,
        /// A Codex `next_turn` mode whose sticky override was accepted with
        /// this exact turn. The daemon uses this fact to durably project the
        /// effective guard before acknowledging the RPC.
        consumed_mode: Option<PermissionMode>,
        /// Exact response or notification-only evidence. Inferred guard
        /// changes restart as Plan until the late exact response agrees.
        confirmation: TurnStartConfirmation,
    },
    ExplicitRefusal {
        message: String,
        /// The staged sticky mode restored for the next safe attempt.
        retained_mode: Option<PermissionMode>,
    },
    /// The request was rejected locally before any native bytes were written.
    /// Retrying unchanged after the named condition clears is safe.
    RetryableNotAccepted { message: String },
    /// This request wrote no native bytes, but another prompt is already
    /// scheduled or awaiting acceptance. Retrying before its authoritative
    /// outcome could duplicate that other prompt.
    ConcurrentNotAccepted { message: String },
    /// No native bytes were written, but this harness generation cannot safely
    /// accept another request (for example its request-id space is exhausted).
    /// The supervisor terminates the generation; a durable staged guard may be
    /// released because the sticky operation is proven not to have run.
    FatalNotAccepted { message: String },
    /// Shared acceptance is uncertain; keep observing without resending the prompt.
    SharedUnknown { message: String },
    Unknown {
        message: String,
        /// The sticky mode included in the request whose acceptance is now
        /// unknowable. Presence requires a durable fail-closed Plan restart.
        attempted_mode: Option<PermissionMode>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStartConfirmation {
    Exact,
    NotificationOnly,
}

/// Opaque, daemon-minted fence for one turn that may consume a queued mode.
/// It is durably armed before the command can reach native stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnGuardAttempt {
    pub token: String,
    pub expected_mode: PermissionMode,
}

/// Immediate result of dispatching a turn request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnStartDispatch {
    /// Claude has no separate acceptance response; Codex also uses this for a
    /// proven local refusal before anything is written.
    Resolved(TurnStartOutcome),
    /// Codex wrote `turn/start`; its exact response will arrive as
    /// [`HarnessEvent::TurnStartResolved`] in native wire order.
    Awaiting,
}

pub enum SteerDispatch {
    Resolved(crate::Result<Delivery>),
    Awaiting,
}

/// Authorization-relevant result of answering a native approval request.
///
/// A session-scoped Claude answer carries a sticky mode change in the same
/// write as the one-shot decision. An I/O error may therefore mean either
/// "nothing happened" or "the looser mode is already live"; callers must not
/// flatten that ambiguity into an ordinary expired-card error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// The shared native request closed; the protocol does not report the winning client.
    Resolved,
    /// Retain the card and observe the shared service without resending the decision.
    AwaitingResolution { message: String },
    Applied {
        /// Exact effective session mode carried by the machine-originated
        /// suggestion, or `None` for a one-shot/native-only decision.
        effective_mode: Option<PermissionMode>,
    },
    /// No native bytes were written, so a sticky guard change is proven absent.
    ExplicitRefusal {
        message: String,
        /// True when the same native card remains pending and may be answered
        /// by an authorized caller; false when it was already stale/consumed.
        retained: bool,
    },
    /// Native input may have been consumed, so the supervisor must terminate
    /// the harness tree before exposing any Unknown. `attempted_mode` decides
    /// whether the daemon must also persist a fail-closed Plan restart.
    Unknown {
        message: String,
        attempted_mode: Option<PermissionMode>,
    },
}

/// Why a harness could not apply a permission-mode change.
///
/// This distinction is authorization state, not presentation detail. The
/// daemon pre-persists a monotonic danger bit before asking a harness to enter
/// bypass mode. It may release that arm only when the harness explicitly says
/// the requested mode did not take effect; an I/O error, timeout, or malformed
/// acknowledgement leaves the real mode unknown and must stay fail-closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionModeChangeError {
    explicit_refusal: bool,
    message: String,
}

impl PermissionModeChangeError {
    pub fn refused(message: impl Into<String>) -> Self {
        Self {
            explicit_refusal: true,
            message: message.into(),
        }
    }

    pub fn outcome_unknown(message: impl Into<String>) -> Self {
        Self {
            explicit_refusal: false,
            message: message.into(),
        }
    }

    pub fn is_explicit_refusal(&self) -> bool {
        self.explicit_refusal
    }
}

impl std::fmt::Display for PermissionModeChangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PermissionModeChangeError {}

pub type PermissionModeChangeResult = Result<PermissionApply, PermissionModeChangeError>;

/// Authorization-relevant result of a runtime permission-mode command.
///
/// The driver-level error distinguishes a proven native refusal from an
/// ambiguous write/receipt failure. The supervisor converts that result into
/// this ticket payload only after performing the lifecycle action promised by
/// each variant: `Unknown` is never observable until the harness process tree
/// has been proven terminated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionModeOutcome {
    SharedApplied {
        applied: PermissionApply,
    },
    SharedUnknown {
        message: String,
    },
    Applied {
        applied: PermissionApply,
    },
    /// Native explicitly rejected the requested mode, proving it did not take
    /// effect. The daemon may release only this command's danger arm.
    ExplicitRefusal {
        message: String,
    },
    /// Native may have consumed the mode change. The supervisor has already
    /// terminated the harness tree; the daemon must durably retain a Plan
    /// restart guard before answering the viewer.
    Unknown {
        message: String,
    },
}

/// A running harness process.
///
/// Explicit variants keep protocol-specific capabilities attached to their drivers.
pub enum AnyDriver {
    ClaudeCode(Box<claude_code::ClaudeCodeDriver>),
    #[cfg(unix)]
    ClaudeNative(Box<claude_native::ClaudeNativeDriver>),
    Codex(Box<codex::CodexDriver>),
    OpenCode(Box<opencode::OpenCodeDriver>),
}

macro_rules! dispatch {
    ($self:expr, $m:ident $(, $a:expr)*) => {
        match $self {
            AnyDriver::ClaudeCode(d) => d.$m($($a),*).await,
            #[cfg(unix)]
            AnyDriver::ClaudeNative(d) => d.$m($($a),*).await,
            AnyDriver::Codex(d) => d.$m($($a),*).await,
            AnyDriver::OpenCode(d) => d.$m($($a),*).await,
        }
    };
}

pub enum InterruptDispatch {
    Resolved(crate::Result<InterruptOutcome>),
    Awaiting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptOutcome {
    Requested,
    NoLongerActive,
}

impl AnyDriver {
    /// Launch a harness. `runtime` must be one of [`drivable`].
    ///
    /// Failure propagates unchanged as [`proc::LaunchError`]: only the layer that
    /// actually calls `Command::spawn` knows whether a process was created; no
    /// layer in between may guess.
    pub async fn launch(runtime: &str, spec: LaunchSpec) -> Result<AnyDriver, proc::LaunchError> {
        #[cfg(unix)]
        if runtime == "claude-code"
            && let Some(session) = spec.resume_from.as_deref()
        {
            if crate::rc::native_claude::Client::available(session, &spec.cwd) {
                return Ok(Self::ClaudeNative(Box::new(
                    claude_native::ClaudeNativeDriver::attach(spec)?,
                )));
            }
            // The terminal admitted at the request may be gone by now; joining is all this
            // opening may do.
            if spec.attach_only {
                return Err(proc::LaunchError::not_spawned(AttachOnlyNotRunning.into()));
            }
            if crate::rc::claude_inbox::session_is_live(session) {
                return Err(proc::LaunchError::not_spawned(anyhow::anyhow!(
                    "native Claude control is still attaching"
                )));
            }
        }
        // Nothing but a native Claude Code terminal can be joined; everything below launches.
        if spec.attach_only {
            return Err(proc::LaunchError::not_spawned(AttachOnlyNotRunning.into()));
        }
        match runtime {
            "claude-code" => Ok(AnyDriver::ClaudeCode(Box::new(
                claude_code::ClaudeCodeDriver::launch(spec).await?,
            ))),
            "codex" => Ok(AnyDriver::Codex(Box::new(
                codex::CodexDriver::launch(spec).await?,
            ))),
            "opencode" => Ok(AnyDriver::OpenCode(Box::new(
                opencode::OpenCodeDriver::launch(spec).await?,
            ))),
            // An unrecognized runtime never even built a command line, so
            // **provably** no process started.
            other => Err(proc::LaunchError::not_spawned(anyhow::anyhow!(
                "{other} has no live control plane — agit can import its transcripts but not drive it remotely.\n  \u{2192} start this session with claude-code, codex, or opencode"
            ))),
        }
    }

    pub fn runtime(&self) -> &'static str {
        match self {
            AnyDriver::ClaudeCode(_) => "claude-code",
            #[cfg(unix)]
            AnyDriver::ClaudeNative(_) => "claude-code",
            AnyDriver::Codex(_) => "codex",
            AnyDriver::OpenCode(_) => "opencode",
        }
    }

    /// Does this harness announce turn boundaries itself?
    ///
    /// codex has a `turn/started` notification, so the supervisor must not emit
    /// its own or the UI draws two turn headers. claude-code has no such frame
    /// (we suppress the `--replay-user-messages` echo of our own message), so
    /// there the supervisor is the only source.
    pub fn emits_turn_started(&self) -> bool {
        match self {
            Self::Codex(_) => true,
            #[cfg(unix)]
            Self::ClaudeNative(_) => true,
            _ => false,
        }
    }

    pub(crate) fn has_active_turn(&self) -> bool {
        match self {
            Self::Codex(driver) => driver.has_active_turn(),
            #[cfg(unix)]
            Self::ClaudeNative(driver) => driver.has_active_turn(),
            _ => false,
        }
    }

    /// The headless harness process, for the one driver whose conversation a terminal can take
    /// over: only headless Claude Code hands a conversation over through a lease.
    pub fn harness_pid(&self) -> Option<u32> {
        match self {
            AnyDriver::ClaudeCode(d) => d.harness_pid(),
            _ => None,
        }
    }

    pub fn runtime_thread_id(&self) -> Option<String> {
        match self {
            AnyDriver::ClaudeCode(d) => d.runtime_thread_id().map(String::from),
            #[cfg(unix)]
            AnyDriver::ClaudeNative(d) => d.runtime_thread_id(),
            AnyDriver::Codex(d) => d.runtime_thread_id().map(String::from),
            AnyDriver::OpenCode(d) => d.runtime_thread_id(),
        }
    }

    pub fn transcript_path(&self) -> Option<PathBuf> {
        match self {
            AnyDriver::ClaudeCode(d) => d.transcript_path(),
            #[cfg(unix)]
            AnyDriver::ClaudeNative(d) => d.transcript_path(),
            AnyDriver::Codex(d) => d.transcript_path(),
            AnyDriver::OpenCode(d) => d.transcript_path(),
        }
    }

    /// A machine-minted native identity binds the accepted prompt to its transcript echo.
    pub fn reserve_prompt_identity(&mut self) -> Option<String> {
        match self {
            AnyDriver::ClaudeCode(driver) => {
                let id = uuid::Uuid::new_v4().to_string();
                driver.next_prompt_id = Some(id.clone());
                Some(id)
            }
            AnyDriver::Codex(driver) => {
                let id = uuid::Uuid::new_v4().to_string();
                driver.next_prompt_id = Some(id.clone());
                Some(id)
            }
            AnyDriver::OpenCode(_) => None,
            #[cfg(unix)]
            AnyDriver::ClaudeNative(_) => None,
        }
    }

    pub async fn start_turn(
        &mut self,
        message: &str,
        consume_pending_mode: bool,
        guard_attempt: Option<TurnGuardAttempt>,
    ) -> TurnStartDispatch {
        match self {
            AnyDriver::ClaudeCode(d) => TurnStartDispatch::Resolved(d.start_turn(message).await),
            #[cfg(unix)]
            AnyDriver::ClaudeNative(d) => d.start_turn(message, guard_attempt).await,
            AnyDriver::OpenCode(d) => d.start_turn(message).await,
            AnyDriver::Codex(d) => {
                d.start_turn(message, consume_pending_mode, guard_attempt)
                    .await
            }
        }
    }

    pub async fn enqueue(
        &mut self,
        request: crate::rc::native_queue::Request,
    ) -> crate::Result<Value> {
        match self {
            AnyDriver::Codex(driver) => driver.enqueue(request).await,
            _ => anyhow::bail!("this runtime does not offer a shared native queue"),
        }
    }

    pub async fn steer(&mut self, message: &str) -> SteerDispatch {
        match self {
            Self::Codex(driver) => SteerDispatch::Resolved(driver.steer(message).await),
            Self::ClaudeCode(driver) => SteerDispatch::Resolved(driver.steer(message).await),
            Self::OpenCode(driver) => SteerDispatch::Resolved(driver.steer(message).await),
            #[cfg(unix)]
            Self::ClaudeNative(driver) => driver.steer(message),
        }
    }

    /// Whether a message for the running turn can wait for that turn's next tool step: only a
    /// native Claude Code terminal whose plugin says so, while a turn runs.
    pub fn steers_at_step(&self) -> bool {
        match self {
            #[cfg(unix)]
            Self::ClaudeNative(driver) => driver.steers_at_step(),
            _ => false,
        }
    }

    /// The turn a native Claude Code terminal last reported running, which a message held for its
    /// next tool step belongs to.
    pub fn running_native_turn(&self) -> Option<String> {
        match self {
            #[cfg(unix)]
            Self::ClaudeNative(driver) => driver.running_turn(),
            _ => None,
        }
    }

    /// Hands `framed` to the running turn's next tool step; see [`Self::steers_at_step`]. Its
    /// operation id comes back with it.
    pub fn steer_at_step(&mut self, framed: &str) -> (SteerDispatch, Option<String>) {
        match self {
            #[cfg(unix)]
            Self::ClaudeNative(driver) => driver.steer_at_step(framed),
            _ => {
                let _ = framed;
                (
                    SteerDispatch::Resolved(Err(anyhow::anyhow!(
                        "this session can't hold a message for the next tool step"
                    ))),
                    None,
                )
            }
        }
    }

    pub fn interrupt_fenced(&self) -> bool {
        match self {
            Self::Codex(_) => true,
            #[cfg(unix)]
            Self::ClaudeNative(_) => true,
            Self::ClaudeCode(_) | Self::OpenCode(_) => false,
        }
    }

    pub async fn interrupt(&mut self, expected_turn_id: Option<&str>) -> InterruptDispatch {
        #[cfg(unix)]
        if let Self::ClaudeNative(driver) = self {
            return driver.interrupt(expected_turn_id);
        }
        InterruptDispatch::Resolved(self.interrupt_managed(expected_turn_id).await)
    }

    async fn interrupt_managed(
        &mut self,
        expected_turn_id: Option<&str>,
    ) -> crate::Result<InterruptOutcome> {
        if let Self::Codex(driver) = self {
            return driver.interrupt_fenced(expected_turn_id).await;
        }
        if let Self::ClaudeCode(driver) = self {
            return driver.interrupt_fenced(expected_turn_id).await;
        }
        anyhow::ensure!(
            expected_turn_id.is_none(),
            "this runtime does not support turn-fenced interrupts"
        );
        match self {
            Self::OpenCode(driver) => driver.interrupt().await?,
            Self::Codex(_) => unreachable!("Codex interrupts use the native turn fence"),
            Self::ClaudeCode(_) => {
                unreachable!("Claude Code refuses turn-fenced interrupts itself")
            }
            #[cfg(unix)]
            Self::ClaudeNative(_) => {
                unreachable!("native Claude interrupts use the native turn fence")
            }
        }
        Ok(InterruptOutcome::Requested)
    }

    pub async fn answer_approval(&mut self, response: &ApprovalResponse) -> ApprovalOutcome {
        dispatch!(self, answer_approval, response)
    }

    /// Forget approvals that the harness abandoned at a turn boundary.
    ///
    /// Both drivers keep native request metadata so a later browser answer can
    /// be translated back to the harness protocol. Once the turn ends or is
    /// interrupted, those native requests cannot be answered anymore; keeping
    /// them would make a stale card look live and would grow this map forever.
    pub fn abandon_pending_approvals(&mut self) -> usize {
        match self {
            AnyDriver::ClaudeCode(d) => d.abandon_pending_approvals(),
            #[cfg(unix)]
            AnyDriver::ClaudeNative(d) => d.abandon_pending_approvals(),
            AnyDriver::Codex(d) => d.abandon_pending_approvals(),
            AnyDriver::OpenCode(d) => d.abandon_pending_approvals(),
        }
    }

    /// Change how much the agent may do unattended. Returns when the change
    /// takes effect — see [`PermissionApply`].
    pub async fn runtime_command(&mut self, name: &str, arguments: Value) -> crate::Result<Value> {
        match self {
            Self::Codex(driver) => driver.runtime_command(name, arguments).await,
            Self::ClaudeCode(driver) => driver.runtime_command(name, arguments).await,
            #[cfg(unix)]
            Self::ClaudeNative(driver) => driver.runtime_command(name, arguments).await,
            _ => anyhow::bail!("This command is not available through the runtime control channel"),
        }
    }

    pub async fn model_control(
        &mut self,
        model: Option<&models::ModelPatch>,
    ) -> crate::Result<Value> {
        match self {
            Self::Codex(d) => d.model_control(model).await,
            Self::ClaudeCode(d) => d.model_control(model).await,
            #[cfg(unix)]
            Self::ClaudeNative(d) => d.model_control(model).await,
            Self::OpenCode(d) => d.model_control(model).await,
        }
    }

    pub async fn set_permission_mode(
        &mut self,
        mode: PermissionMode,
    ) -> PermissionModeChangeResult {
        dispatch!(self, set_permission_mode, mode)
    }

    pub fn permission_mode(&self) -> PermissionMode {
        match self {
            AnyDriver::ClaudeCode(d) => d.permission_mode(),
            #[cfg(unix)]
            AnyDriver::ClaudeNative(d) => d.permission_mode(),
            AnyDriver::Codex(d) => d.permission_mode(),
            AnyDriver::OpenCode(d) => d.permission_mode(),
        }
    }

    pub fn shared_executor(&self) -> bool {
        match self {
            Self::Codex(driver) => driver.shared_executor(),
            #[cfg(unix)]
            Self::ClaudeNative(_) => true,
            _ => false,
        }
    }

    /// Whether launching this driver started the runtime's process. A Claude Code terminal
    /// attached for native control was already running, and so was a shared Codex server the
    /// launch found listening.
    pub fn started_process(&self) -> bool {
        match self {
            Self::Codex(driver) => driver.started_process(),
            #[cfg(unix)]
            Self::ClaudeNative(_) => false,
            _ => true,
        }
    }

    pub(crate) fn correlates_pending_prompt(&self) -> bool {
        #[cfg(unix)]
        if matches!(self, Self::ClaudeNative(_)) {
            return false;
        }
        true
    }

    pub fn permission_mode_known(&self) -> bool {
        #[cfg(unix)]
        if matches!(self, Self::ClaudeNative(_)) {
            // SessionStart observations do not cover every local permission change.
            return false;
        }
        !matches!(self, Self::Codex(driver) if !driver.permission_mode_known())
    }

    pub async fn next_event(&mut self) -> Option<HarnessEvent> {
        dispatch!(self, next_event)
    }

    pub fn take_native_snapshot(&mut self) -> Option<opencode::NativeSnapshot> {
        match self {
            Self::OpenCode(driver) => driver.take_snapshot(),
            _ => None,
        }
    }

    pub async fn shutdown(&mut self) -> crate::Result<()> {
        dispatch!(self, shutdown)
    }
}

/// Serialized bytes the slash-command catalog of one runtime may take in the machine's description.
/// The description travels as one frame a controller bounds, and a catalog holding every command
/// and Skill a person installed, each with its description, can outgrow it.
const COMMANDS_BUDGET: usize = 24 * 1024;
/// The longest description a catalog keeps, then the shorter ones tried until it fits. Zero keeps
/// the names alone.
const COMMAND_DESCRIPTION_CHARS: [usize; 5] = [200, 120, 80, 40, 0];
const COMMAND_HINT_CHARS: usize = 80;

/// `commands` as a machine's description carries them, within [`COMMANDS_BUDGET`]. Descriptions and
/// argument hints are shortened as far as the catalog needs, every command at one length, down to
/// the names alone: a command's name is what autocomplete needs, and the runtime's built-in
/// commands sit among the person's own in no useful order, so shortening keeps them all where
/// cutting the list would lose some. Only when the names alone do not fit are commands kept in
/// order while they fit. Returns the commands kept and how many were left out.
pub(crate) fn bounded_commands(commands: Vec<SlashCommand>) -> (Vec<SlashCommand>, usize) {
    fn cut(text: &str, chars: usize) -> Option<String> {
        if chars == 0 {
            return None;
        }
        if text.chars().count() <= chars {
            return Some(text.to_owned());
        }
        let mut cut: String = text.chars().take(chars - 1).collect();
        cut.push('…');
        Some(cut)
    }
    // Its encoding and the comma before the next one.
    let bytes = |command: &SlashCommand| {
        serde_json::to_string(command).map_or(usize::MAX, |text| text.len() + 1)
    };
    let total = commands.len();
    let mut shortened = Vec::new();
    for chars in COMMAND_DESCRIPTION_CHARS {
        shortened = commands
            .iter()
            .map(|command| SlashCommand {
                name: command.name.clone(),
                description: command
                    .description
                    .as_deref()
                    .and_then(|text| cut(text, chars)),
                argument_hint: command
                    .argument_hint
                    .as_deref()
                    .and_then(|text| cut(text, chars.min(COMMAND_HINT_CHARS))),
            })
            .collect();
        let size = shortened
            .iter()
            .fold(0usize, |size, command| size.saturating_add(bytes(command)));
        if size <= COMMANDS_BUDGET {
            return (shortened, 0);
        }
    }
    let mut used = 0usize;
    shortened.retain(|command| {
        used = used.saturating_add(bytes(command));
        used <= COMMANDS_BUDGET
    });
    let omitted = total - shortened.len();
    (shortened, omitted)
}

/// Static capability description for a runtime, reported at register time.
pub fn capability_of(runtime: &str) -> RuntimeCapability {
    match runtime {
        "claude-code" => claude_code::capability(),
        "codex" => codex::capability(),
        "opencode" => opencode::capability(),
        other => RuntimeCapability {
            runtime: other.to_string(),
            available: false,
            version: None,
            steer: None,
            interrupt: false,
            approvals: false,
            partial_messages: false,
            resume: false,
            commands: vec![],
            commands_omitted: 0,
            permission_modes: vec![],
            permission_switch: None,
        },
    }
}

/// Every runtime with an implemented live control plane.
/// Import support alone cannot advertise remote execution capabilities.
pub fn drivable() -> Vec<RuntimeCapability> {
    vec![
        claude_code::capability(),
        codex::capability(),
        opencode::capability(),
    ]
}

/// `<cli> --version`, best effort, used for the capability report.
pub fn probe_version(cli: &str, args: &[&str]) -> Option<String> {
    let executable = crate::adapter::which(cli)?;
    let out = crate::infra::background::command(executable)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Extract a first-line summary for an approval card: the command, the file
/// being written, whatever the tool's input makes obvious. Falls back to the
/// tool name so the card is never blank.
pub fn summarize_tool(tool: &str, input: &Value) -> String {
    let get = |k: &str| input.get(k).and_then(|v| v.as_str());
    if let Some(cmd) = get("command") {
        return truncate(cmd, 200);
    }
    if let Some(p) = get("file_path").or_else(|| get("path")) {
        return format!("{tool} {}", truncate(p, 160));
    }
    if let Some(pat) = get("pattern") {
        return format!("{tool} {}", truncate(pat, 160));
    }
    if let Some(u) = get("url") {
        return format!("{tool} {}", truncate(u, 160));
    }
    tool.to_string()
}

/// Paths a tool call touches, for the approval card and the audit row.
pub fn paths_of(input: &Value) -> Vec<String> {
    let mut out = vec![];
    for k in ["file_path", "path", "notebook_path"] {
        if let Some(s) = input.get(k).and_then(|v| v.as_str()) {
            out.push(s.to_string());
        }
    }
    if let Some(arr) = input.get("edits").and_then(|v| v.as_array()) {
        for e in arr {
            if let Some(s) = e.get("file_path").and_then(|v| v.as_str()) {
                out.push(s.to_string());
            }
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(index: usize, description_chars: usize) -> SlashCommand {
        SlashCommand {
            name: format!("/skill-{index}"),
            description: Some("d".repeat(description_chars)),
            argument_hint: Some("h".repeat(120)),
        }
    }

    /// The command catalog a machine describes stays within its byte budget however many commands
    /// and Skills a person installed. A catalog that fits keeps its descriptions, cut to the
    /// longest length; a larger one shortens them as far as it needs and keeps every command, so a
    /// built-in listed late is not lost to Skills listed early; only when the names alone do not
    /// fit are commands left out, in order, and counted. Sent unbounded, a large catalog grows the
    /// description past what a controller accepts and the machine shows offline; cut by position
    /// alone, it loses whichever commands come last.
    #[test]
    fn a_runtimes_command_catalog_stays_within_its_budget() {
        let size = |kept: &[SlashCommand]| serde_json::to_string(kept).unwrap().len();
        let longest = COMMAND_DESCRIPTION_CHARS[0];

        // Fits at the longest length: descriptions are cut there, hints at theirs, and all stay.
        let (kept, omitted) = bounded_commands((0..10).map(|i| command(i, 500)).collect());
        assert_eq!((kept.len(), omitted), (10, 0));
        let description = kept[0].description.as_deref().unwrap();
        assert_eq!(description.chars().count(), longest);
        assert!(description.ends_with('…'));
        assert_eq!(
            kept[0].argument_hint.as_deref().unwrap().chars().count(),
            COMMAND_HINT_CHARS
        );

        // Over the budget at the longest length: shorter descriptions, every command kept, the
        // last one included.
        let (kept, omitted) = bounded_commands((0..150).map(|i| command(i, 500)).collect());
        assert_eq!((kept.len(), omitted), (150, 0));
        assert!(size(&kept) <= COMMANDS_BUDGET);
        assert_eq!(kept[149].name, "/skill-149");
        assert!(kept[0].description.as_deref().unwrap().chars().count() < longest);

        // The names alone do not fit: kept in order while they fit, the rest counted.
        let (kept, omitted) = bounded_commands((0..2000).map(|i| command(i, 500)).collect());
        assert!(!kept.is_empty() && omitted > 0);
        assert_eq!(kept.len() + omitted, 2000);
        assert!(size(&kept) <= COMMANDS_BUDGET);
        assert_eq!(kept[0].name, "/skill-0");
        assert!(kept[0].description.is_none() && kept[0].argument_hint.is_none());

        // At the budget: the commands that fit are all kept, and one more is left out.
        let each = serde_json::to_string(&SlashCommand {
            name: "/skill-0".into(),
            description: None,
            argument_hint: None,
        })
        .unwrap()
        .len()
            + 1;
        let fit = COMMANDS_BUDGET / each;
        let named = |count: usize| -> Vec<SlashCommand> {
            (0..count).map(|i| command(i % 10, 500)).collect()
        };
        assert_eq!(bounded_commands(named(fit)).1, 0);
        assert_eq!(bounded_commands(named(fit + 1)).1, 1);
    }

    #[test]
    fn bounded_turn_ids_fail_stop_without_evicting_or_rejecting_duplicates() {
        let max_id = "x".repeat(MAX_NATIVE_TURN_ID_BYTES);
        let too_long = "x".repeat(MAX_NATIVE_TURN_ID_BYTES + 1);
        let mut ids = BoundedTurnIds::with_limits(2, max_id.len() + 1);

        assert_eq!(ids.try_insert(max_id.clone()), Ok(true));
        assert_eq!(ids.try_insert(max_id.clone()), Ok(false));
        assert!(matches!(
            ids.try_insert(too_long),
            Err(NativeTurnIdentityError::TooLong { .. })
        ));
        assert_eq!(ids.try_insert("y".into()), Ok(true));
        assert_eq!(ids.payload_bytes(), max_id.len() + 1);
        assert_eq!(ids.try_insert("y".into()), Ok(false));
        assert!(matches!(
            ids.try_insert("z".into()),
            Err(NativeTurnIdentityError::EntryLimit { max: 2 })
        ));
        assert!(ids.contains(&max_id));
        assert!(ids.contains("y"));
        assert_eq!(ids.len(), 2);

        let mut payload_limited = BoundedTurnIds::with_limits(8, 3);
        assert_eq!(payload_limited.try_insert("abc".into()), Ok(true));
        assert!(matches!(
            payload_limited.try_insert("d".into()),
            Err(NativeTurnIdentityError::PayloadLimit { .. })
        ));
        assert_eq!(payload_limited.payload_bytes(), 3);
        assert!(!payload_limited.contains("d"));

        let mut next_generation = BoundedTurnIds::with_limits(1, 3);
        assert_eq!(next_generation.try_insert("abc".into()), Ok(true));
        assert_eq!(next_generation.len(), 1);
    }

    #[test]
    fn driver_lineage_env_never_exposes_a_partial_identity() {
        let unbound: std::collections::BTreeMap<_, _> = lineage_env(None).into_iter().collect();
        assert_eq!(
            unbound.get(SUPERVISED_HOOK_ENV).map(String::as_str),
            Some("1"),
            "every RC Stop hook delegates, even without repository lineage"
        );
        assert!(!unbound.contains_key("AGIT_SESSION"));
        assert!(!unbound.contains_key(crate::hub::identity::EXPECTED_AGENT_ID_ENV));
        let lineage = crate::rc::lineage::AgitSession::new(
            "alice/payments",
            "00000000-0000-0000-0000-000000000001",
            "s/1",
        )
        .unwrap();
        let env: std::collections::BTreeMap<_, _> =
            lineage_env(Some(&lineage)).into_iter().collect();
        assert_eq!(
            env.get("AGIT_SESSION").map(String::as_str),
            Some("alice/payments@s/1")
        );
        assert_eq!(
            env.get(crate::hub::identity::EXPECTED_AGENT_ID_ENV)
                .map(String::as_str),
            Some("00000000-0000-0000-0000-000000000001")
        );
        assert_eq!(env.get(SUPERVISED_HOOK_ENV).map(String::as_str), Some("1"));
    }

    #[test]
    fn summaries_prefer_the_thing_a_human_would_check() {
        let bash = serde_json::json!({"command": "rm -rf ./build", "description": "clean"});
        assert_eq!(summarize_tool("Bash", &bash), "rm -rf ./build");

        let write = serde_json::json!({"file_path": "/srv/app/src/main.rs", "content": "…"});
        assert_eq!(
            summarize_tool("Write", &write),
            "Write /srv/app/src/main.rs"
        );

        // Unknown shape still names the tool rather than rendering an empty card.
        assert_eq!(
            summarize_tool("Mystery", &serde_json::json!({"x":1})),
            "Mystery"
        );
    }

    #[test]
    fn paths_include_multi_edit_targets() {
        let multi = serde_json::json!({"edits":[{"file_path":"a.rs"},{"file_path":"b.rs"}]});
        assert_eq!(paths_of(&multi), vec!["a.rs", "b.rs"]);
    }

    #[test]
    fn drivable_runtimes_advertise_their_control_planes() {
        let d = drivable();
        assert_eq!(
            d.iter().map(|c| c.runtime.as_str()).collect::<Vec<_>>(),
            vec!["claude-code", "codex", "opencode"]
        );
        assert!(
            d.iter().all(|c| c.interrupt),
            "drivable runtimes can be interrupted"
        );
        // cursor is importable but has no control plane.
        assert!(!capability_of("cursor").interrupt);
    }
}
