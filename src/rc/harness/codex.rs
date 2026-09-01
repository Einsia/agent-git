//! codex driver — app-server JSON-RPC over stdio.
//!
//! # Why this one is the easier of the two
//!
//! codex ships a real bidirectional protocol. `turn/steer` and `turn/interrupt`
//! are first-class verbs, approvals are server→client requests with typed
//! decisions, and items arrive already structured. Our own hub protocol is
//! deliberately named after it (`turn.steer`, `item.completed`) because that
//! vocabulary has been validated by a real client (the VS Code extension), and
//! copying it keeps a future `codex --remote` interop cheap.
//!
//! # The one trap
//!
//! **codex omits the `"jsonrpc":"2.0"` field on the wire.** Its own
//! `app-server-protocol/src/rpc.rs` says so in a comment ("We do not do true
//! JSON-RPC 2.0, as we neither send nor expect the jsonrpc field"), and the
//! generated schema has no such property. So a strict JSON-RPC parser —
//! `jsonrpsee-types`, for one — cannot read codex traffic at all. We hand-roll
//! a lenient reader here and keep our own strict `"2.0"` on the hub link, where
//! we control both ends.
//!
//! Method names below were read out of `codex app-server generate-ts` on the
//! installed binary (codex-cli 0.147.0), not from documentation.

use super::proc::{LaunchError, Line, Proc, Pushback};
use super::{
    ApprovalOutcome, BoundedTurnIds, HarnessEvent, LaunchSpec, PermissionModeChangeResult,
    TurnGuardAttempt, TurnOutcome, TurnStartConfirmation, TurnStartDispatch, TurnStartOutcome,
    validate_native_turn_id,
};
use crate::protocol::{
    ApprovalDecision, ApprovalKind, ApprovalRequest, ApprovalResponse, ApprovalScope, Delivery,
    ItemKind, PermissionApply, PermissionMode, RuntimeCapability,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;

const TURN_START_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// The `turn/steer` response has to be waited for as well — see [`CodexDriver::steer`].
const STEER_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const MAX_RETIRED_TURN_STARTS: usize = 4_096;
const MAX_RETIRED_TURN_START_PAYLOAD_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub(super) struct RequestIdExhausted;

impl std::fmt::Display for RequestIdExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("codex native request-id space is exhausted")
    }
}

impl std::error::Error for RequestIdExhausted {}

pub fn capability() -> RuntimeCapability {
    RuntimeCapability {
        runtime: "codex".into(),
        available: crate::adapter::which("codex").is_some(),
        version: super::probe_version("codex", &["--version"]),
        // A protocol verb, so it lands at once rather than at a tool boundary.
        steer: Some(Delivery::Immediate),
        interrupt: true,
        approvals: true,
        partial_messages: true,
        resume: true,
        commands: vec![],
        // No `acceptEdits`: codex's approval policy does not separate edits
        // from commands (that split is a claude-code concept), and offering a
        // mode we would have to approximate is worse than not offering it —
        // the user would set "auto-accept edits" and still get prompted.
        permission_modes: vec![
            PermissionMode::Default,
            PermissionMode::Auto,
            PermissionMode::Plan,
            PermissionMode::Bypass,
        ],
        // codex has no live equivalent of `set_permission_mode`: the policy
        // rides along with `turn/start` as a sticky override ("this turn and
        // subsequent turns"), so a switch cannot land until the next turn.
        permission_switch: Some(PermissionApply::NextTurn),
    }
}

/// [`PermissionMode`] → codex's two axes.
///
/// codex splits what claude-code keeps in one scalar: `approvalPolicy` decides
/// whether a human is asked, `sandbox` decides what the process may touch even
/// when nobody is asked. Both have to move together or the mode is a lie —
/// `never` with a read-only sandbox is not "auto", it is "fails silently".
fn native_policy(mode: PermissionMode) -> (&'static str, &'static str) {
    match mode {
        // Ask on request: the whole point of RC is that a human somewhere gets
        // to decide.
        PermissionMode::Default => ("on-request", "workspace-write"),
        // No neutral spelling for codex; treated as Default by `open_thread`.
        PermissionMode::AcceptEdits => ("on-request", "workspace-write"),
        PermissionMode::Auto => ("never", "workspace-write"),
        PermissionMode::Plan => ("never", "read-only"),
        PermissionMode::Bypass => ("never", "danger-full-access"),
    }
}

/// The `turn/start` spelling of the sandbox, which is a tagged object rather
/// than the string `thread/start` takes. Getting these two confused is the
/// easiest mistake in this file.
fn turn_sandbox_policy(mode: PermissionMode) -> Value {
    match native_policy(mode).1 {
        "read-only" => json!({"type": "readOnly"}),
        "danger-full-access" => json!({"type": "dangerFullAccess"}),
        _ => json!({"type": "workspaceWrite"}),
    }
}

/// One approval awaiting an answer.
struct PendingApproval {
    /// Which server request the answer goes back to.
    req_id: Value,
    /// Which method asked — it decides the **shape** of the response, not just the decision
    /// value.
    method: String,
    /// The `RequestPermissionProfile` from an `item/permissions/requestApproval` request.
    /// Granted back unchanged as a `GrantedPermissionProfile`.
    requested_permissions: Option<Value>,
}

#[derive(Debug)]
struct PendingTurnStart {
    request_id: i64,
    staged_mode: Option<PermissionMode>,
    guard_attempt: Option<TurnGuardAttempt>,
    deadline: tokio::time::Instant,
    /// A native notification is independent proof that this request landed,
    /// even if its client response is delayed or the stream then closes.
    observed_turn_id: Option<String>,
    completed: bool,
}

#[derive(Debug)]
struct RetiredTurnStart {
    turn_id: String,
    confirmation_token: Option<String>,
    consumed_mode: Option<PermissionMode>,
}

#[derive(Debug)]
struct RetiredTurnStartError {
    message: String,
    attempted_mode: Option<PermissionMode>,
    confirmation_token: Option<String>,
}

#[derive(Debug)]
struct RetiredTurnStarts {
    entries: HashMap<i64, RetiredTurnStart>,
    payload_bytes: usize,
    max_entries: usize,
    max_payload_bytes: usize,
}

impl Default for RetiredTurnStarts {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            payload_bytes: 0,
            max_entries: MAX_RETIRED_TURN_STARTS,
            max_payload_bytes: MAX_RETIRED_TURN_START_PAYLOAD_BYTES,
        }
    }
}

impl RetiredTurnStarts {
    fn try_insert(&mut self, request_id: i64, retired: RetiredTurnStart) -> Result<(), String> {
        if self.entries.contains_key(&request_id) {
            return Err(format!(
                "codex reused retired turn/start request id {request_id}"
            ));
        }
        validate_native_turn_id(&retired.turn_id)
            .map_err(|error| format!("codex retired turn/start: {error}"))?;
        if self.entries.len() >= self.max_entries {
            return Err(format!(
                "codex retired turn/start budget reached {} entries",
                self.max_entries
            ));
        }
        let confirmation_bytes = retired.confirmation_token.as_ref().map_or(0, String::len);
        let added = retired
            .turn_id
            .len()
            .checked_add(confirmation_bytes)
            .ok_or_else(|| "codex retired turn/start payload size overflowed".to_string())?;
        let payload_bytes = self
            .payload_bytes
            .checked_add(added)
            .ok_or_else(|| "codex retired turn/start payload size overflowed".to_string())?;
        if payload_bytes > self.max_payload_bytes {
            return Err(format!(
                "codex retired turn/start payload would reach {payload_bytes} bytes; maximum is {}",
                self.max_payload_bytes
            ));
        }
        let replaced = self.entries.insert(request_id, retired);
        debug_assert!(replaced.is_none(), "duplicate request id was checked above");
        self.payload_bytes = payload_bytes;
        Ok(())
    }

    fn take(&mut self, request_id: i64) -> Option<RetiredTurnStart> {
        let retired = self.entries.remove(&request_id)?;
        let confirmation_bytes = retired.confirmation_token.as_ref().map_or(0, String::len);
        let removed = retired
            .turn_id
            .len()
            .checked_add(confirmation_bytes)
            .expect("stored retired payload was already measured");
        self.payload_bytes = self
            .payload_bytes
            .checked_sub(removed)
            .expect("stored retired payload accounting stays balanced");
        Some(retired)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    fn with_limits(max_entries: usize, max_payload_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            payload_bytes: 0,
            max_entries,
            max_payload_bytes,
        }
    }
}

/// Classify only the response to this exact client request id.
///
/// A JSON-RPC error proves rejection. Silence, EOF, or a malformed success does
/// not prove either outcome because codex may already have accepted the turn.
fn turn_start_response(
    value: &Value,
    request_id: i64,
    staged_mode: Option<PermissionMode>,
) -> Option<TurnStartOutcome> {
    if value.get("method").is_some() || value.get("id").and_then(Value::as_i64) != Some(request_id)
    {
        return None;
    }

    match (value.get("result"), value.get("error")) {
        (Some(result), None) => {
            let turn_id = result
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
                .filter(|turn_id| !turn_id.is_empty());
            Some(match turn_id {
                Some(turn_id) => TurnStartOutcome::Accepted {
                    turn_id: turn_id.to_string(),
                    still_running: true,
                    consumed_mode: staged_mode,
                    confirmation: TurnStartConfirmation::Exact,
                },
                None => TurnStartOutcome::Unknown {
                    message: "codex acknowledged turn/start without reporting the accepted turn id"
                        .into(),
                    attempted_mode: staged_mode,
                },
            })
        }
        (None, Some(error)) => Some(TurnStartOutcome::ExplicitRefusal {
            message: format!(
                "codex refused turn/start: {}",
                error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given")
            ),
            retained_mode: staged_mode,
        }),
        _ => Some(TurnStartOutcome::Unknown {
            message: "codex returned a malformed turn/start response".into(),
            attempted_mode: staged_mode,
        }),
    }
}

pub struct CodexDriver {
    proc: Proc,
    thread_id: Option<String>,
    current_turn: Option<String>,
    /// Our own JSON-RPC id counter for client→server requests.
    next_id: Option<i64>,
    /// approval_id → what is needed to answer that approval.
    pending_approvals: HashMap<String, PendingApproval>,
    cwd: PathBuf,
    resume_from: Option<String>,
    started: bool,
    /// The handshake-chain request still awaiting its response: `(request id, method name)`.
    ///
    /// A usable session has to get through `initialize` → `thread/start`|`thread/resume`.
    /// If any link in that chain is **refused natively**, `thread_id` is None forever and
    /// everything downstream hangs off it: no `Ready`, the queued first turn is not
    /// dispatched, the transcript has no tailer, and every later `turn.start` answers only
    /// "the thread is still opening, retry shortly". Such a response taking the generic error
    /// branch is written to tracing and nowhere else, while `session.resume` has already
    /// returned success — what ships is a dead session, idle forever, dragging a live
    /// `codex app-server` child, with nobody told. So remember this request's id, to
    /// recognize its error and treat it as fatal.
    handshake_request: Option<(i64, &'static str)>,
    /// Current permission mode, and the one to apply at the next `turn/start`.
    ///
    /// Two fields because codex's switch is not live: a viewer can ask for
    /// `auto` while a turn is running, and the honest answer is "queued for the
    /// next turn". Reporting `mode` as already changed would make the UI claim
    /// a guard was lifted while the running turn is still asking for approvals.
    mode: PermissionMode,
    pending_mode: Option<PermissionMode>,
    /// Exact request currently awaiting native acceptance. Its response is
    /// consumed by `next_event`, beside all intervening notifications, rather
    /// than by a blocking command handler.
    pending_turn_start: Option<PendingTurnStart>,
    /// Starts resolved from authoritative notifications before their exact
    /// response arrived. Late responses are checked once and ignored. Entries
    /// remain until their exact response arrives or this harness exits:
    /// evicting an unresolved request id would make its eventual response look
    /// unrelated and either kill a healthy session or miss a contradiction.
    retired_turn_starts: RetiredTurnStarts,
    /// Completed native ids. Codex notifications and server requests
    /// can arrive after the exact start response; neither is allowed to revive
    /// a turn whose authoritative completion has already been observed. Keep
    /// them for the harness lifetime: native delivery has no bounded lateness.
    completed_turn_ids: BoundedTurnIds,
    /// Lines picked up while waiting for the `turn/steer` response, not yet handed upward.
    ///
    /// The steer response shares one stdout with notifications and approvals. Dropping the
    /// lines in between leaves a gap in the transcript and an approval card that never pops
    /// up; so they are held here and `next_event` releases them first next time — order
    /// unchanged. The same-named field in the claude driver is the same mechanism.
    pushback: Pushback,
}

impl CodexDriver {
    /// Start one codex. Failure comes back as [`LaunchError`], carrying "did the OS spawn
    /// happen" unchanged: a `Proc::spawn` error is **provably never started**, while a
    /// handshake write that fails means the process is already running.
    pub async fn launch(spec: LaunchSpec) -> Result<CodexDriver, LaunchError> {
        let mut env: Vec<(String, String)> = vec![("AGIT_RC".into(), "1".into())];
        // codex tags every upstream request with an `originator` header, and it
        // uses a *different* value for `app-server` (`codex_app_server`) than for
        // the interactive CLI (`codex_cli_rs`). Relays and gateways commonly
        // allowlist only the CLI value, so driving codex through app-server gets
        // a 403 that reads as an auth problem and is not one. Present ourselves
        // as the CLI unless the operator has already pinned a value.
        if std::env::var_os("CODEX_INTERNAL_ORIGINATOR_OVERRIDE").is_none() {
            env.push((
                "CODEX_INTERNAL_ORIGINATOR_OVERRIDE".into(),
                "codex_cli_rs".into(),
            ));
        }
        env.extend(super::lineage_env(spec.agit_session.as_ref()));
        let proc = Proc::spawn("codex", &["app-server".to_string()], &spec.cwd, &env)?;
        let mut d = CodexDriver {
            proc,
            thread_id: None,
            current_turn: None,
            next_id: Some(1),
            pending_approvals: Default::default(),
            cwd: spec.cwd.clone(),
            resume_from: spec.resume_from.clone(),
            started: false,
            handshake_request: None,
            mode: spec.effective_mode(),
            pending_mode: None,
            pending_turn_start: None,
            retired_turn_starts: Default::default(),
            completed_turn_ids: Default::default(),
            pushback: Default::default(),
        };
        // Handshake. `experimentalApi` is required for the thread/turn surface.
        // The child is already running by this point: a handshake failure is uniformly
        // recorded as having crossed the spawn boundary.
        let id = d
            .alloc_id()
            .map_err(|e| LaunchError::spawned(anyhow::Error::new(e)))?;
        d.send(&json!({
            "id": id,
            "method": "initialize",
            "params": {
                "clientInfo": {"name":"agit","title":"AgentGit RC","version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"experimentalApi": true, "requestAttestation": false}
            }
        }))
        .await
        .map_err(LaunchError::spawned)?;
        d.handshake_request = Some((id, "initialize"));
        Ok(d)
    }

    fn alloc_id(&mut self) -> Result<i64, RequestIdExhausted> {
        let id = self.next_id.take().ok_or(RequestIdExhausted)?;
        self.next_id = id.checked_add(1);
        Ok(id)
    }

    async fn send(&mut self, v: &Value) -> crate::Result<()> {
        self.proc.write_line(v).await
    }

    /// Open the thread once `initialize` has been answered.
    async fn open_thread(&mut self) -> crate::Result<()> {
        if self.started {
            return Ok(());
        }
        let id = self.alloc_id()?;
        let (policy, sandbox) = native_policy(self.mode);
        // Both branches carry the policy. `thread/resume` used to omit it, which
        // meant a resumed session silently fell back to whatever the user's
        // `config.toml` says — so "take this conversation over from the web"
        // could come back with a different guard than it had, in either
        // direction, and nothing on screen would say so.
        let (method, params) = match &self.resume_from {
            Some(tid) => (
                "thread/resume",
                json!({
                    "threadId": tid,
                    "cwd": self.cwd.to_string_lossy(),
                    "approvalPolicy": policy,
                    "sandbox": sandbox
                }),
            ),
            None => (
                "thread/start",
                json!({
                    "cwd": self.cwd.to_string_lossy(),
                    "approvalPolicy": policy,
                    "sandbox": sandbox
                }),
            ),
        };
        self.send(&json!({"id": id, "method": method, "params": params}))
            .await?;
        self.started = true;
        self.handshake_request = Some((id, method));
        Ok(())
    }

    pub fn runtime_thread_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    /// codex writes `~/.codex/sessions/YYYY/MM/DD/rollout-<ISO>-<uuid>.jsonl`.
    /// The date path is not knowable up front, so resolve through the adapter's
    /// sqlite index (0.4 ms) once the thread id is known.
    pub fn transcript_path(&self) -> Option<PathBuf> {
        let tid = self.thread_id.as_ref()?;
        crate::adapter::get("codex")
            .ok()?
            .resolve(tid, Some(&self.cwd))
    }

    /// Send one turn without consuming native stdout in this command path.
    ///
    /// The exact response is classified by [`next_event`](Self::next_event),
    /// beside approvals and notifications. This keeps their native order and
    /// avoids freezing the supervisor for the response timeout.
    pub async fn start_turn(
        &mut self,
        message: &str,
        consume_pending_mode: bool,
        guard_attempt: Option<TurnGuardAttempt>,
    ) -> TurnStartDispatch {
        if self.pending_turn_start.is_some() {
            return TurnStartDispatch::Resolved(TurnStartOutcome::RetryableNotAccepted {
                message: "another Codex turn is still awaiting native acceptance".into(),
            });
        }
        if self.current_turn.is_some() {
            return TurnStartDispatch::Resolved(TurnStartOutcome::RetryableNotAccepted {
                message: "a Codex turn is already running; steer it or wait for completion".into(),
            });
        }
        let Some(tid) = self.thread_id.clone() else {
            return TurnStartDispatch::Resolved(TurnStartOutcome::RetryableNotAccepted {
                message: "the Codex thread is still opening; retry after it is ready".into(),
            });
        };
        // Inspect the queued mode without consuming it. Request-id exhaustion
        // is a fatal generation-local condition, but it occurs before native
        // I/O and therefore must leave the sticky mode honestly unconsumed.
        let staged_mode = consume_pending_mode.then_some(self.pending_mode).flatten();
        let guard_attempt = match (staged_mode, guard_attempt) {
            (Some(mode), Some(attempt)) if attempt.expected_mode == mode => Some(attempt),
            (Some(_mode), _) => {
                return TurnStartDispatch::Resolved(TurnStartOutcome::RetryableNotAccepted {
                    message: "the queued mode and durable guard-attempt fence do not match".into(),
                });
            }
            (None, None) => None,
            (None, Some(_)) => {
                return TurnStartDispatch::Resolved(TurnStartOutcome::RetryableNotAccepted {
                    message: "the daemon armed a guard attempt that the Codex driver cannot apply"
                        .into(),
                });
            }
        };
        let id = match self.alloc_id() {
            Ok(id) => id,
            Err(error) => {
                return TurnStartDispatch::Resolved(TurnStartOutcome::FatalNotAccepted {
                    message: error.to_string(),
                });
            }
        };
        if staged_mode.is_some() {
            let consumed = self.pending_mode.take();
            debug_assert_eq!(consumed, staged_mode);
        }
        let mut params = json!({
            "threadId": tid,
            "input": [{"type":"text","text": message, "text_elements": []}]
        });
        // A queued mode change lands here, because this is the only place codex
        // lets it land. The override is sticky ("this turn and subsequent
        // turns"), so it is sent once and then becomes the session's mode.
        if let Some(next) = staged_mode {
            let (policy, _) = native_policy(next);
            params["approvalPolicy"] = json!(policy);
            params["sandboxPolicy"] = turn_sandbox_policy(next);
        }
        if let Err(error) = self
            .send(&json!({"id": id, "method": "turn/start", "params": params}))
            .await
        {
            // `write_all` can fail after a prefix reached the child. Retrying
            // could duplicate both the prompt and sticky mode, so the session
            // must be retired by the supervisor.
            return TurnStartDispatch::Resolved(TurnStartOutcome::Unknown {
                message: format!("codex turn/start outcome is unknown: {error}"),
                attempted_mode: staged_mode,
            });
        }
        self.pending_turn_start = Some(PendingTurnStart {
            request_id: id,
            staged_mode,
            guard_attempt,
            deadline: tokio::time::Instant::now() + TURN_START_RESPONSE_TIMEOUT,
            observed_turn_id: None,
            completed: false,
        });
        TurnStartDispatch::Awaiting
    }

    fn restore_staged_mode(&mut self, staged_mode: Option<PermissionMode>) {
        if self.pending_mode.is_none() {
            self.pending_mode = staged_mode;
        }
    }

    fn turn_identity_invariant(&self, message: impl Into<String>) -> HarnessEvent {
        HarnessEvent::ProtocolInvariant {
            message: message.into(),
            attempted_mode: self
                .pending_turn_start
                .as_ref()
                .and_then(|pending| pending.staged_mode),
            confirmation_token: None,
        }
    }

    /// Validate and record one piece of evidence for the live native turn.
    ///
    /// `Ok(true)` establishes a previously unknown active turn; `Ok(false)`
    /// repeats the same identity. Any different id is a fatal protocol
    /// contradiction and leaves both `current_turn` and pending acceptance
    /// metadata untouched.
    fn observe_active_turn_identity(
        &mut self,
        turn_id: &str,
        evidence: &str,
    ) -> Result<bool, String> {
        validate_native_turn_id(turn_id).map_err(|error| format!("codex {evidence}: {error}"))?;
        if self.completed_turn(turn_id) {
            return Err(format!(
                "codex {evidence} referenced already completed turn {turn_id}"
            ));
        }
        if let Some(pending) = self.pending_turn_start.as_ref() {
            if pending.completed {
                return Err(format!(
                    "codex {evidence} arrived after the pending turn had completed"
                ));
            }
            if let Some(observed) = pending.observed_turn_id.as_deref()
                && observed != turn_id
            {
                return Err(format!(
                    "codex {evidence} for turn {turn_id} contradicted pending turn {observed}"
                ));
            }
        }
        if let Some(active) = self.current_turn.as_deref()
            && active != turn_id
        {
            return Err(format!(
                "codex {evidence} for turn {turn_id} contradicted active turn {active}"
            ));
        }

        if let Some(pending) = self.pending_turn_start.as_mut()
            && pending.observed_turn_id.is_none()
        {
            pending.observed_turn_id = Some(turn_id.to_string());
        }
        let established = self.current_turn.is_none();
        if established {
            self.current_turn = Some(turn_id.to_string());
        }
        Ok(established)
    }

    fn completed_turn(&self, turn_id: &str) -> bool {
        self.completed_turn_ids.contains(turn_id)
    }

    fn remember_completed_turn(&mut self, turn_id: String) -> Result<bool, String> {
        self.completed_turn_ids
            .try_insert(turn_id)
            .map_err(|error| format!("codex completed-turn tombstone: {error}"))
    }

    /// Queue a permission-mode change for the next turn.
    ///
    /// codex has no live switch — the full `ClientRequest` surface was
    /// enumerated and there is no `set_permission_mode` equivalent; the policy
    /// only travels on `thread/*` and `turn/start`. So this stores the intent
    /// and reports [`PermissionApply::NextTurn`], and the UI says so rather
    /// than pretending the guard already moved.
    pub async fn set_permission_mode(
        &mut self,
        mode: PermissionMode,
    ) -> PermissionModeChangeResult {
        self.pending_mode = Some(mode);
        Ok(PermissionApply::NextTurn)
    }

    pub fn permission_mode(&self) -> PermissionMode {
        // The pending one is not yet in force; report what is actually true.
        self.mode
    }

    fn outcome_after_turn_response_loss(
        &mut self,
        pending: PendingTurnStart,
        message: String,
    ) -> Result<TurnStartOutcome, RetiredTurnStartError> {
        if let Some(turn_id) = pending.observed_turn_id {
            let confirmation_token = pending
                .guard_attempt
                .as_ref()
                .map(|attempt| attempt.token.clone());
            let retired = RetiredTurnStart {
                turn_id: turn_id.clone(),
                confirmation_token: confirmation_token.clone(),
                consumed_mode: pending.staged_mode,
            };
            if let Err(message) = self
                .retired_turn_starts
                .try_insert(pending.request_id, retired)
            {
                return Err(RetiredTurnStartError {
                    message,
                    attempted_mode: pending.staged_mode,
                    confirmation_token,
                });
            }
            if let Some(mode) = pending.staged_mode {
                self.mode = mode;
            }
            self.current_turn = (!pending.completed).then(|| turn_id.clone());
            Ok(TurnStartOutcome::Accepted {
                turn_id,
                still_running: !pending.completed,
                consumed_mode: pending.staged_mode,
                confirmation: TurnStartConfirmation::NotificationOnly,
            })
        } else {
            Ok(TurnStartOutcome::Unknown {
                message,
                attempted_mode: pending.staged_mode,
            })
        }
    }

    fn turn_response_loss_event(
        &mut self,
        pending: PendingTurnStart,
        message: String,
    ) -> HarnessEvent {
        match self.outcome_after_turn_response_loss(pending, message) {
            Ok(outcome) => HarnessEvent::TurnStartResolved(outcome),
            Err(error) => HarnessEvent::ProtocolInvariant {
                message: error.message,
                attempted_mode: error.attempted_mode,
                confirmation_token: error.confirmation_token,
            },
        }
    }

    pub async fn steer(&mut self, message: &str) -> crate::Result<Delivery> {
        let (Some(tid), Some(turn)) = (self.thread_id.clone(), self.current_turn.clone()) else {
            anyhow::bail!("no turn is running — send a message instead of steering");
        };
        let id = self.alloc_id()?;
        self.send(&json!({
            "id": id,
            "method": "turn/steer",
            "params": {
                "threadId": tid,
                "expectedTurnId": turn,
                "input": [{"type":"text","text": message, "text_elements": []}]
            }
        }))
        .await?;
        // **Wait for codex to answer before saying "delivered".**
        //
        // `turn/steer` does get refused outright by the native side (an expectedTurnId that
        // has already ended, a turn just wrapping up ... all come back as a JSON-RPC error).
        // Reporting `Delivery::Immediate` on a successful write alone reports a refusal as
        // delivered — the user believes the course changed while the agent never received
        // that sentence and keeps running as before. So a refusal has to come back along
        // this RPC.
        //
        // Other lines (deltas, approvals, notifications) still arrive on the stream while
        // waiting; dropping them leaves a gap in the transcript — hold them in `pushback`,
        // and `next_event` releases them in their original order.
        let deadline = tokio::time::Instant::now() + STEER_RESPONSE_TIMEOUT;
        loop {
            let queued = match tokio::time::timeout_at(deadline, self.proc.next()).await {
                Ok(Some(queued)) => queued,
                Ok(None) => {
                    anyhow::bail!("codex exited before answering turn/steer — delivery unknown")
                }
                Err(_) => anyhow::bail!(
                    "codex did not answer turn/steer within {}s — delivery unknown",
                    STEER_RESPONSE_TIMEOUT.as_secs()
                ),
            };
            if let Line::Json(v) = queued.line()
                && v.get("method").is_none()
                && v.get("id").and_then(Value::as_i64) == Some(id)
            {
                if let Some(error) = v.get("error") {
                    anyhow::bail!(
                        "codex refused turn/steer: {}",
                        error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("no reason given")
                    );
                }
                if v.get("result").is_some() {
                    return Ok(Delivery::Immediate);
                }
                anyhow::bail!(
                    "codex answered turn/steer without result or error — delivery unknown"
                );
            }
            // Not the line being waited for: keep it, do not drop it. EOF is queued too,
            // so `next_event` still takes its usual `Exited` path.
            let eof = matches!(queued.line(), Line::Eof);
            self.pushback.push(queued);
            if eof {
                anyhow::bail!("codex exited before answering turn/steer — delivery unknown");
            }
        }
    }

    pub async fn interrupt(&mut self) -> crate::Result<()> {
        // `current_turn == None` does not always mean "nothing is running": in the window
        // where `turn/start` has been written to native stdin and its response has not come
        // back (at most TURN_START_RESPONSE_TIMEOUT), the turn is most likely already
        // running and only lacks a usable turn id to fill `turn/interrupt`. Returning Ok
        // here reports "dropped" as "stopped" — the user pressed stop and the agent keeps
        // going. With no id there is no interrupt to send, and the only honest answer is an
        // error that lets the caller retry. `pending.completed` is the exception: that turn
        // already has an authoritative `turn/completed`, so there really is nothing to stop.
        if self.current_turn.is_none()
            && let Some(pending) = self.pending_turn_start.as_ref()
            && !pending.completed
        {
            anyhow::bail!(
                "a turn is still awaiting native acceptance — retry the interrupt once it is confirmed"
            );
        }
        let (Some(tid), Some(turn)) = (self.thread_id.clone(), self.current_turn.clone()) else {
            return Ok(()); // nothing running: interrupting is a no-op, not an error
        };
        let id = self.alloc_id()?;
        self.send(&json!({"id": id, "method":"turn/interrupt","params":{"threadId": tid, "turnId": turn}}))
            .await
    }

    pub fn abandon_pending_approvals(&mut self) -> usize {
        let abandoned = self.pending_approvals.len();
        self.pending_approvals.clear();
        abandoned
    }

    pub async fn answer_approval(&mut self, r: &ApprovalResponse) -> ApprovalOutcome {
        let Some(pending) = self.pending_approvals.remove(&r.approval_id) else {
            return ApprovalOutcome::ExplicitRefusal {
                message: format!(
                    "approval {} is no longer pending (it timed out or was already answered)",
                    r.approval_id
                ),
                retained: false,
            };
        };
        let req_id = pending.req_id.clone();
        // Which method asked decides the *shape* of the answer, not just the
        // decision value.
        let method = pending.method.clone();

        // `item/permissions/requestApproval` is the odd one out: its result has
        // **no `decision` field at all** (the schema's required list is
        // `["permissions"]`). We were answering every method with
        // `{"decision": …}`, which for this one is a malformed response — the
        // request then never resolves and the turn hangs.
        if method.starts_with("item/permissions/") {
            let body = match r.decision {
                // **Grant the profile from the request unchanged.**
                //
                // This response has no decision field (the schema's required list
                // is only `permissions`), so the profile itself is the only carrier
                // of what was approved. Returning an empty object grants nothing —
                // which is exactly how this protocol expresses a refusal, so the
                // owner presses "allow", codex receives zero authority and can only
                // keep failing or asking again, while the interface already shows
                // it as approved.
                //
                // What is granted is **the one it asked for**, no more and no less:
                // whether that overreaches was already decided by `requires_owner`
                // on the machine side, and the human pressed allow looking at that
                // same content.
                ApprovalDecision::Allow => json!({
                    "permissions": pending.requested_permissions.unwrap_or(json!({})),
                    "scope": if r.scope == ApprovalScope::Session { "session" } else { "turn" }
                }),
                // Granting nothing is how a refusal is expressed here; there is
                // no "deny" value to send.
                ApprovalDecision::Deny => json!({"permissions": {}, "scope": "turn"}),
            };
            return match self.send(&json!({"id": req_id, "result": body})).await {
                Ok(()) => ApprovalOutcome::Applied {
                    effective_mode: None,
                },
                Err(error) => ApprovalOutcome::Unknown {
                    message: format!("codex approval outcome is unknown: {error}"),
                    attempted_mode: None,
                },
            };
        }

        let decision = match (r.decision, r.scope) {
            // "Allow, and stop asking" is a first-class decision value here
            // rather than a mode change — codex caches it per session itself.
            (ApprovalDecision::Allow, ApprovalScope::Session) => "acceptForSession",
            (ApprovalDecision::Allow, _) => "accept",
            // `decline` refuses this call but lets the turn continue, which is
            // what a reviewer saying "not that one" means. `cancel` (abort the
            // whole turn) is what the interrupt button is for.
            (ApprovalDecision::Deny, _) => "decline",
        };
        match self
            .send(&json!({"id": req_id, "result": {"decision": decision}}))
            .await
        {
            Ok(()) => ApprovalOutcome::Applied {
                effective_mode: None,
            },
            Err(error) => ApprovalOutcome::Unknown {
                message: format!("codex approval outcome is unknown: {error}"),
                attempted_mode: None,
            },
        }
    }

    pub async fn next_event(&mut self) -> Option<HarnessEvent> {
        loop {
            // Release what was held while waiting for the `turn/steer` response first,
            // then read new lines — the order is the stream's order.
            let line = if let Some(line) = self.pushback.pop_front() {
                line
            } else if let Some(pending) = self.pending_turn_start.as_ref() {
                match tokio::time::timeout_at(pending.deadline, self.proc.next()).await {
                    Ok(Some(queued)) => queued.into_line(),
                    Ok(None) => {
                        let pending = self
                            .pending_turn_start
                            .take()
                            .expect("pending turn checked above");
                        return Some(self.turn_response_loss_event(
                            pending,
                            "codex exited before confirming turn/start".into(),
                        ));
                    }
                    Err(_) => {
                        let pending = self
                            .pending_turn_start
                            .take()
                            .expect("pending turn checked above");
                        return Some(self.turn_response_loss_event(
                            pending,
                            format!(
                                "codex did not confirm turn/start within {}s",
                                TURN_START_RESPONSE_TIMEOUT.as_secs()
                            ),
                        ));
                    }
                }
            } else {
                self.proc.next().await?.into_line()
            };
            let v = match line {
                Line::Eof => {
                    if let Some(pending) = self.pending_turn_start.take() {
                        return Some(self.turn_response_loss_event(
                            pending,
                            "codex exited before confirming turn/start".into(),
                        ));
                    }
                    return Some(HarnessEvent::Exited {
                        code: self.proc.wait().await,
                    });
                }
                Line::Notice(t) => return Some(HarnessEvent::Notice { text: t }),
                // A critical frame (`turn/completed`, an approval request, ...)
                // that could not be held while waiting for a response. Reporting it
                // as an ordinary notice leaves the supervisor stuck on Running with
                // a session that can never see its turn end; this must fail-stop the
                // whole generation.
                Line::Fatal(message) => return Some(self.turn_identity_invariant(message)),
                Line::Json(v) => v,
            };
            match self.classify(v).await {
                Some(ev) => return Some(ev),
                None => continue,
            }
        }
    }

    async fn classify(&mut self, v: Value) -> Option<HarnessEvent> {
        if let Some(pending) = self.pending_turn_start.as_ref()
            && v.get("method").is_none()
            && v.get("id").and_then(Value::as_i64) == Some(pending.request_id)
            && let Some(turn_id) = v
                .get("result")
                .and_then(|result| result.get("turn"))
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
            && let Err(error) = validate_native_turn_id(turn_id)
        {
            return Some(
                self.turn_identity_invariant(format!("codex turn/start response: {error}")),
            );
        }
        if let Some(pending) = self.pending_turn_start.as_ref()
            && let Some(mut outcome) =
                turn_start_response(&v, pending.request_id, pending.staged_mode)
        {
            let accepted_turn_id = match &outcome {
                TurnStartOutcome::Accepted { turn_id, .. } => Some(turn_id.as_str()),
                _ => None,
            };
            let contradicts_observed = pending
                .observed_turn_id
                .as_deref()
                .is_some_and(|observed| accepted_turn_id != Some(observed));
            let contradicts_active = accepted_turn_id.is_some_and(|turn_id| {
                self.current_turn
                    .as_deref()
                    .is_some_and(|active| active != turn_id)
            });
            let resurrects_completed = accepted_turn_id
                .is_some_and(|turn_id| self.completed_turn(turn_id) && !pending.completed);
            let pending = self
                .pending_turn_start
                .take()
                .expect("the matching response has a pending request");
            if contradicts_observed || contradicts_active || resurrects_completed {
                outcome = TurnStartOutcome::Unknown {
                    message: "codex turn/start response contradicted the active turn lifecycle"
                        .into(),
                    attempted_mode: pending.staged_mode,
                };
            }
            match &mut outcome {
                TurnStartOutcome::Accepted {
                    turn_id,
                    still_running,
                    consumed_mode,
                    ..
                } => {
                    *still_running = !pending.completed;
                    self.current_turn = (*still_running).then(|| turn_id.clone());
                    if let Some(mode) = consumed_mode {
                        self.mode = *mode;
                    }
                }
                TurnStartOutcome::ExplicitRefusal { .. } => {
                    self.restore_staged_mode(pending.staged_mode);
                    if let TurnStartOutcome::ExplicitRefusal { retained_mode, .. } = &mut outcome {
                        *retained_mode = self.pending_mode;
                    }
                }
                TurnStartOutcome::RetryableNotAccepted { .. }
                | TurnStartOutcome::ConcurrentNotAccepted { .. }
                | TurnStartOutcome::FatalNotAccepted { .. }
                | TurnStartOutcome::Unknown { .. } => {}
            }
            return Some(HarnessEvent::TurnStartResolved(outcome));
        }

        if v.get("method").is_none()
            && let Some(request_id) = v.get("id").and_then(Value::as_i64)
            && let Some(retired) = self.retired_turn_starts.take(request_id)
        {
            let agrees = v.get("error").is_none()
                && v.get("result")
                    .and_then(|result| result.get("turn"))
                    .and_then(|turn| turn.get("id"))
                    .and_then(Value::as_str)
                    == Some(retired.turn_id.as_str());
            return if agrees {
                retired.confirmation_token.map(|confirmation_token| {
                    HarnessEvent::TurnStartConfirmed {
                        confirmation_token: Some(confirmation_token),
                        effective_mode: retired.consumed_mode,
                    }
                })
            } else {
                Some(HarnessEvent::ProtocolInvariant {
                    message: format!(
                        "late codex turn/start response {request_id} contradicted accepted turn {}",
                        retired.turn_id
                    ),
                    // This invariant belongs to the retired request. Its
                    // opaque token is the only trusted guard identity; a newer
                    // pending turn may carry an unrelated mode.
                    attempted_mode: None,
                    confirmation_token: retired.confirmation_token,
                })
            };
        }

        // Response to one of our requests (has `id` + `result`, no `method`).
        if v.get("method").is_none() && v.get("id").is_some() {
            // The handshake-chain response is claimed here first. **A refusal is fatal**:
            // `thread_id` never gets a value, so no `Ready`, no first-turn dispatch, no
            // transcript tailing, and every later `turn.start` answers only "the thread is
            // still opening" — while `session.start`/`session.resume` returned success long
            // ago. Degraded to a Notice, what ships is a session idle forever, dragging a
            // live child, that nobody knows is already dead; a `thread/resume` whose rollout
            // has been cleaned up lands exactly here:
            // `{"error":{"code":-32600,"message":"no rollout found for thread id ..."}}`.
            if let Some((id, method)) = self.handshake_request
                && v.get("id").and_then(Value::as_i64) == Some(id)
            {
                self.handshake_request = None;
                if let Some(error) = v.get("error") {
                    return Some(HarnessEvent::ProtocolInvariant {
                        message: format!(
                            "codex refused {method}: {}",
                            error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("no reason given")
                        ),
                        // No turn yet, so no consumed mode and no guard token
                        // to account for.
                        attempted_mode: None,
                        confirmation_token: None,
                    });
                }
            }
            if let Some(res) = v.get("result") {
                // initialize → open the thread
                if !self.started {
                    return match self.open_thread().await {
                        Ok(()) => None,
                        Err(error) => Some(HarnessEvent::ProtocolInvariant {
                            message: format!("codex could not open its native thread: {error}"),
                            attempted_mode: None,
                            confirmation_token: None,
                        }),
                    };
                }
                if let Some(t) = res
                    .get("thread")
                    .and_then(|t| t.get("id"))
                    .and_then(|x| x.as_str())
                {
                    if self.thread_id.as_deref() == Some(t) {
                        return None; // already announced by thread/started
                    }
                    self.thread_id = Some(t.to_string());
                    let path = self.transcript_path();
                    return Some(HarnessEvent::Ready {
                        runtime_thread_id: t.to_string(),
                        transcript_path: path,
                    });
                }
                if let Some(turn_id) = res
                    .get("turn")
                    .and_then(|turn| turn.get("id"))
                    .and_then(Value::as_str)
                {
                    return Some(HarnessEvent::ProtocolInvariant {
                        message: format!(
                            "unmatched codex turn/start response reported turn {turn_id}"
                        ),
                        attempted_mode: self
                            .pending_turn_start
                            .as_ref()
                            .and_then(|pending| pending.staged_mode),
                        confirmation_token: None,
                    });
                }
            }
            if let Some(e) = v.get("error") {
                return Some(HarnessEvent::Notice {
                    text: format!(
                        "codex error: {}",
                        e.get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown")
                    ),
                });
            }
            return None;
        }

        let method = v.get("method").and_then(|x| x.as_str()).unwrap_or("");
        let params = v.get("params").cloned().unwrap_or(Value::Null);

        // Server→client request: needs a response, so it must carry an id.
        if v.get("id").is_some() {
            return self.classify_server_request(method, &params, v.get("id").cloned().unwrap());
        }

        match method {
            "thread/started" => {
                let t = params
                    .get("threadId")
                    .and_then(|x| x.as_str())
                    .or_else(|| {
                        params
                            .get("thread")
                            .and_then(|t| t.get("id"))
                            .and_then(|x| x.as_str())
                    })?
                    .to_string();
                if self.thread_id.as_deref() == Some(t.as_str()) {
                    return None; // already announced by the thread/start response
                }
                self.thread_id = Some(t.clone());
                let path = self.transcript_path();
                Some(HarnessEvent::Ready {
                    runtime_thread_id: t,
                    transcript_path: path,
                })
            }
            "turn/started" => {
                let Some(turn) = params
                    .get("turn")
                    .and_then(|turn| turn.get("id"))
                    .and_then(Value::as_str)
                else {
                    return Some(
                        self.turn_identity_invariant("codex turn/started omitted its turn id"),
                    );
                };
                if let Err(error) = validate_native_turn_id(turn) {
                    return Some(
                        self.turn_identity_invariant(format!("codex turn/started: {error}")),
                    );
                }
                let turn = turn.to_string();
                if self.completed_turn(&turn) {
                    // Passive delivery is unbounded. Once A completed, a late
                    // duplicate head for A is unrelated to a now-live B.
                    return None;
                }
                match self.observe_active_turn_identity(&turn, "turn/started") {
                    Ok(true) => Some(HarnessEvent::TurnStarted {
                        turn_id: turn,
                        prompt: None,
                    }),
                    Ok(false) => None,
                    Err(message) => Some(self.turn_identity_invariant(message)),
                }
            }
            "turn/completed" => {
                let Some(turn) = params.get("turn") else {
                    return Some(
                        self.turn_identity_invariant("codex turn/completed omitted its turn"),
                    );
                };
                let Some(id) = turn.get("id").and_then(|x| x.as_str()) else {
                    return Some(
                        self.turn_identity_invariant("codex turn/completed omitted its turn id"),
                    );
                };
                if let Err(error) = validate_native_turn_id(id) {
                    return Some(
                        self.turn_identity_invariant(format!("codex turn/completed: {error}")),
                    );
                }
                let id = id.to_string();
                let status = turn
                    .get("status")
                    .and_then(|x| x.as_str())
                    .unwrap_or("completed");
                if self.completed_turn(&id) {
                    // A passive duplicate for a completed A cannot settle or
                    // perturb a later active B.
                    return None;
                }
                if let Some(pending) = self.pending_turn_start.as_ref() {
                    if pending.completed {
                        return Some(self.turn_identity_invariant(format!(
                            "codex turn/completed repeated pending turn {id} without its completion tombstone"
                        )));
                    }
                    if let Some(observed) = pending.observed_turn_id.as_deref()
                        && observed != id
                    {
                        return Some(self.turn_identity_invariant(format!(
                            "codex turn/completed for {id} contradicted pending turn {observed}"
                        )));
                    }
                }
                match self.current_turn.as_deref() {
                    Some(active) if active != id => {
                        return Some(self.turn_identity_invariant(format!(
                            "codex turn/completed for {id} contradicted active turn {active}"
                        )));
                    }
                    None if self.pending_turn_start.is_none() => {
                        return Some(self.turn_identity_invariant(format!(
                            "codex turn/completed for {id} had no active or pending turn"
                        )));
                    }
                    _ => {}
                }

                // Reserve the lifetime tombstone before clearing any live
                // lifecycle state. If the bounded identity budget is spent,
                // fail-stop with the active turn and pending approval maps
                // completely untouched.
                if let Err(message) = self.remember_completed_turn(id.clone()) {
                    return Some(self.turn_identity_invariant(message));
                }

                if let Some(pending) = self.pending_turn_start.as_mut() {
                    if pending.observed_turn_id.is_none() {
                        pending.observed_turn_id = Some(id.clone());
                    }
                    pending.completed = true;
                }
                if self.current_turn.as_deref() == Some(id.as_str()) {
                    self.current_turn = None;
                }
                Some(HarnessEvent::TurnCompleted {
                    turn_id: id,
                    outcome: match status {
                        "interrupted" => TurnOutcome::Interrupted,
                        "failed" => TurnOutcome::Error,
                        _ => TurnOutcome::Ok,
                    },
                    cost_usd: None,
                    duration_ms: turn.get("durationMs").and_then(|x| x.as_u64()),
                })
            }
            "item/started" => {
                let item = params.get("item")?;
                let id = item.get("id").and_then(|x| x.as_str())?.to_string();
                let (kind, tool) = item_kind(item);
                Some(HarnessEvent::ItemStarted {
                    item_id: id,
                    kind,
                    tool,
                })
            }
            "item/completed" => {
                let id = params.get("item")?.get("id")?.as_str()?.to_string();
                Some(HarnessEvent::ItemCompleted { item_id: id })
            }
            "item/agentMessage/delta"
            | "item/reasoning/textDelta"
            | "item/reasoning/summaryTextDelta"
            | "item/plan/delta" => {
                let id = params.get("itemId").and_then(|x| x.as_str())?.to_string();
                let text = params.get("delta").and_then(|x| x.as_str())?.to_string();
                Some(HarnessEvent::Delta { item_id: id, text })
            }
            "item/commandExecution/outputDelta" | "item/fileChange/outputDelta" => {
                let id = params.get("itemId").and_then(|x| x.as_str())?.to_string();
                // Output chunks arrive base64 in some builds and plain in others.
                let text = params
                    .get("chunk")
                    .or_else(|| params.get("delta"))
                    .and_then(|x| x.as_str())?
                    .to_string();
                Some(HarnessEvent::Delta { item_id: id, text })
            }
            // Surface the whole payload: codex puts the useful part in different
            // keys depending on the error class, and a bare "codex error" in the
            // log is exactly the kind of message that wastes an afternoon.
            "error" => Some(HarnessEvent::Notice {
                text: format!(
                    "codex error: {}",
                    params
                        .get("message")
                        .and_then(|x| x.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| serde_json::to_string(&params).unwrap_or_default())
                ),
            }),
            _ => None,
        }
    }

    fn classify_server_request(
        &mut self,
        method: &str,
        params: &Value,
        req_id: Value,
    ) -> Option<HarnessEvent> {
        let kind = match method {
            "item/commandExecution/requestApproval" => ApprovalKind::Exec,
            "item/fileChange/requestApproval" => ApprovalKind::FileChange,
            "item/permissions/requestApproval" => ApprovalKind::PermissionEscalation,
            _ => return None,
        };
        let Some(turn_id) = params
            .get("turnId")
            .and_then(Value::as_str)
            .filter(|turn_id| !turn_id.is_empty())
        else {
            return Some(
                self.turn_identity_invariant(format!("codex {method} omitted a valid turnId")),
            );
        };
        if let Err(error) = validate_native_turn_id(turn_id) {
            return Some(self.turn_identity_invariant(format!("codex {method}: {error}")));
        }
        if self.completed_turn(turn_id) {
            return Some(HarnessEvent::ProtocolInvariant {
                message: format!(
                    "codex approval arrived after turn {turn_id} had already completed"
                ),
                attempted_mode: None,
                confirmation_token: None,
            });
        }
        if let Err(message) = self.observe_active_turn_identity(turn_id, "approval request") {
            return Some(self.turn_identity_invariant(message));
        }
        // This key must be **unique per request**, because the whole `pending_approvals`
        // map is how "which request id to answer" is found again.
        //
        // `approvalId` is optional — codex's schema says outright that this field is null
        // for ordinary shell / unified_exec approvals, and the fileChange / permissions
        // params do not carry it at all. Falling back to `itemId` is not enough: **several**
        // callbacks can hang off one parent itemId (the zsh-exec-bridge case, written in the
        // schema comments), so two concurrent approvals compute the same key and the second
        // insert evicts the first one's request id — the first request is then never
        // answered, codex waits indefinitely, and the whole turn hangs.
        //
        // So the server request id is spliced in: by the JSON-RPC definition it is unique
        // per request.
        let approval_id = match params.get("approvalId").and_then(|x| x.as_str()) {
            Some(a) => a.to_string(),
            None => {
                let item = params
                    .get("itemId")
                    .and_then(|x| x.as_str())
                    .unwrap_or("item");
                format!("{item}:{req_id}")
            }
        };
        self.pending_approvals.insert(
            approval_id.clone(),
            PendingApproval {
                req_id,
                method: method.to_string(),
                // A permission-escalation approval keeps the profile from the
                // request — approving grants it back unchanged.
                requested_permissions: params.get("permissions").cloned(),
            },
        );

        let command = params
            .get("command")
            .and_then(|x| x.as_str())
            .unwrap_or_default();
        let summary = if command.is_empty() {
            params
                .get("reason")
                .and_then(|x| x.as_str())
                .unwrap_or(method)
                .to_string()
        } else {
            command.to_string()
        };
        Some(HarnessEvent::Approval(ApprovalRequest {
            approval_id,
            session_id: String::new(),
            turn_id: turn_id.to_string(),
            kind,
            tool: match kind {
                ApprovalKind::Exec => "shell".into(),
                ApprovalKind::FileChange => "apply_patch".into(),
                ApprovalKind::PermissionEscalation => "permissions".into(),
            },
            input: params.clone(),
            summary,
            paths: params
                .get("grantRoot")
                .and_then(|x| x.as_str())
                .map(|s| vec![s.to_string()])
                .unwrap_or_default(),
            timeout_secs: 0,
            // As above: the real decision belongs to the supervisor; this is the
            // fail-closed initial value.
            requires_owner: true,
            // The supervisor overwrites these two fields with the real test before
            // redaction. The initial value here is the fail-closed side.
            owner_reason: Some(crate::protocol::OwnerReason::Unprovable),
            // Codex has sticky native answers (`acceptForSession` / session
            // permission grants) but exposes no exact neutral mode for their
            // restart policy. Until that effect is modelled, advertising the
            // button would promise a guard the daemon cannot authorize or
            // durably reproduce.
            can_allow_for_session: false,
            suggested_permission_mode: None,
            requested_at: chrono::Utc::now().to_rfc3339(),
        }))
    }

    pub async fn shutdown(&mut self) -> crate::Result<()> {
        self.proc.shutdown().await
    }

    #[cfg(test)]
    pub(crate) fn test_responder(thread_id: Option<&str>, responses: &[Value]) -> CodexDriver {
        let cwd = PathBuf::from("/");
        let responses = responses
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let script = concat!(
            "IFS= read -r _request\n",
            "printf '%s\\n' \"$AGIT_CODEX_TEST_RESPONSES\"\n",
            "while IFS= read -r _rest; do :; done"
        );
        CodexDriver {
            proc: Proc::spawn(
                "sh",
                &["-c".to_string(), script.to_string()],
                &cwd,
                &[("AGIT_CODEX_TEST_RESPONSES".into(), responses)],
            )
            .expect("test responder"),
            thread_id: thread_id.map(String::from),
            current_turn: None,
            next_id: Some(1),
            pending_approvals: Default::default(),
            cwd,
            resume_from: None,
            started: thread_id.is_some(),
            handshake_request: None,
            mode: PermissionMode::Default,
            pending_mode: None,
            pending_turn_start: None,
            retired_turn_starts: Default::default(),
            completed_turn_ids: Default::default(),
            pushback: Default::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_test_thread_id(&mut self, thread_id: &str) {
        self.thread_id = Some(thread_id.to_string());
        self.started = true;
    }

    #[cfg(test)]
    pub(crate) fn exhaust_test_request_ids(&mut self) {
        self.next_id = None;
    }

    #[cfg(test)]
    pub(crate) fn set_test_current_turn(&mut self, turn_id: &str) {
        self.current_turn = Some(turn_id.to_string());
    }

    #[cfg(test)]
    pub(crate) fn fail_test_shutdowns(
        &mut self,
        count: usize,
    ) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        self.proc.fail_shutdowns(count)
    }
}

/// codex `ThreadItem` → our coarse streaming kind.
fn item_kind(item: &Value) -> (ItemKind, Option<String>) {
    match item.get("type").and_then(|x| x.as_str()).unwrap_or("") {
        "userMessage" => (ItemKind::UserMessage, None),
        "agentMessage" | "plan" => (ItemKind::AssistantMessage, None),
        "reasoning" => (ItemKind::Reasoning, None),
        "commandExecution" => (
            ItemKind::ToolCall,
            item.get("command")
                .and_then(|x| x.as_str())
                .map(|_| "shell".to_string()),
        ),
        "fileChange" => (ItemKind::ToolCall, Some("apply_patch".into())),
        "mcpToolCall" => (
            ItemKind::ToolCall,
            item.get("tool").and_then(|x| x.as_str()).map(String::from),
        ),
        "dynamicToolCall" => (
            ItemKind::ToolCall,
            item.get("tool").and_then(|x| x.as_str()).map(String::from),
        ),
        _ => (ItemKind::Other, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe() -> CodexDriver {
        let cwd = PathBuf::from("/");
        CodexDriver {
            proc: Proc::spawn("cat", &[], &cwd, &[]).expect("test process"),
            thread_id: None,
            current_turn: None,
            next_id: Some(1),
            pending_approvals: Default::default(),
            cwd,
            resume_from: None,
            started: false,
            handshake_request: None,
            mode: PermissionMode::Default,
            pending_mode: None,
            pending_turn_start: None,
            retired_turn_starts: Default::default(),
            completed_turn_ids: Default::default(),
            pushback: Default::default(),
        }
    }

    fn probe_with_responses(responses: &[Value]) -> CodexDriver {
        CodexDriver::test_responder(Some("thread-1"), responses)
    }

    fn guard_attempt(mode: PermissionMode) -> TurnGuardAttempt {
        TurnGuardAttempt {
            token: "guard-attempt".into(),
            expected_mode: mode,
        }
    }

    #[tokio::test]
    async fn request_ids_reach_i64_max_once_then_turn_start_fails_before_write() {
        let mut driver = probe();
        driver.set_test_thread_id("thread-1");
        driver.next_id = Some(i64::MAX - 1);
        assert_eq!(driver.alloc_id().expect("penultimate id"), i64::MAX - 1);
        assert_eq!(driver.alloc_id().expect("last unique id"), i64::MAX);
        assert!(driver.alloc_id().is_err());

        driver.pending_mode = Some(PermissionMode::Plan);
        assert!(matches!(
            driver
                .start_turn("never written", true, Some(guard_attempt(PermissionMode::Plan)))
                .await,
            TurnStartDispatch::Resolved(TurnStartOutcome::FatalNotAccepted { message })
                if message.contains("request-id space")
        ));
        assert_eq!(
            driver.pending_mode,
            Some(PermissionMode::Plan),
            "fatal pre-write exhaustion cannot consume a sticky queued mode"
        );
        assert!(driver.pending_turn_start.is_none());
        assert!(driver.current_turn.is_none());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), driver.proc.next())
                .await
                .is_err(),
            "no native request is written after allocator exhaustion"
        );
        driver.shutdown().await.expect("stop test process");
    }

    #[tokio::test]
    async fn i64_max_request_id_can_accept_one_last_turn_before_exhaustion() {
        let mut driver = probe_with_responses(&[
            json!({"id": i64::MAX, "result": {"turn": {"id": "last-turn"}}}),
            json!({
                "method": "turn/completed",
                "params": {"turn": {"id": "last-turn", "status": "completed"}}
            }),
        ]);
        driver.next_id = Some(i64::MAX);
        assert_eq!(
            driver.start_turn("last unique request", true, None).await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnStartResolved(TurnStartOutcome::Accepted {
                turn_id,
                ..
            })) if turn_id == "last-turn"
        ));
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnCompleted { turn_id, .. }) if turn_id == "last-turn"
        ));
        assert!(matches!(
            driver.start_turn("one too many", true, None).await,
            TurnStartDispatch::Resolved(TurnStartOutcome::FatalNotAccepted { .. })
        ));
        driver.shutdown().await.expect("stop test process");
    }

    #[tokio::test]
    async fn steer_and_interrupt_report_typed_request_id_exhaustion_before_write() {
        for operation in ["steer", "interrupt"] {
            let mut driver = probe();
            driver.set_test_thread_id("thread-1");
            driver.current_turn = Some("turn-1".into());
            if operation == "interrupt" {
                driver.pending_approvals.insert(
                    "approval-1".into(),
                    PendingApproval {
                        req_id: json!(7),
                        method: "item/commandExecution/requestApproval".into(),
                        requested_permissions: None,
                    },
                );
            }
            driver.exhaust_test_request_ids();
            let error = if operation == "steer" {
                driver.steer("never written").await.unwrap_err()
            } else {
                driver.interrupt().await.unwrap_err()
            };
            assert!(crate::rc::harness::is_request_id_exhaustion(&error));
            assert_eq!(driver.current_turn.as_deref(), Some("turn-1"));
            if operation == "interrupt" {
                assert!(
                    driver.pending_approvals.contains_key("approval-1"),
                    "a pre-write interrupt failure cannot abandon native approvals"
                );
            }
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(20), driver.proc.next())
                    .await
                    .is_err(),
                "{operation} exhaustion must precede native I/O"
            );
            driver.shutdown().await.expect("stop test process");
        }
    }

    #[tokio::test]
    async fn open_thread_exhaustion_is_fatal_but_server_approval_ids_remain_answerable() {
        let mut opening = probe();
        opening.exhaust_test_request_ids();
        assert!(matches!(
            opening.classify(json!({"id": 1, "result": {}})).await,
            Some(HarnessEvent::ProtocolInvariant { message, .. })
                if message.contains("request-id space")
        ));
        assert!(!opening.started);
        assert!(opening.thread_id.is_none());
        opening.shutdown().await.expect("stop opening test process");

        let mut approval = probe();
        assert!(matches!(
            approval.classify_server_request(
                "item/commandExecution/requestApproval",
                &json!({
                    "approvalId": "approval-1",
                    "itemId": "item-1",
                    "turnId": "turn-1",
                    "command": "cargo test"
                }),
                json!("server-request-id"),
            ),
            Some(HarnessEvent::Approval(_))
        ));
        approval.exhaust_test_request_ids();
        assert!(matches!(
            approval
                .answer_approval(&ApprovalResponse {
                    approval_id: "approval-1".into(),
                    session_id: "session-1".into(),
                    decision: ApprovalDecision::Deny,
                    scope: ApprovalScope::Once,
                    message: None,
                    by: Some("operator".into()),
                })
                .await,
            ApprovalOutcome::Applied {
                effective_mode: None
            }
        ));
        approval
            .shutdown()
            .await
            .expect("stop approval test process");
    }

    /// A handshake-chain request that is **refused natively** is fatal on the spot rather
    /// than degraded into a Notice.
    ///
    /// A Notice is written to tracing and nowhere else, so `thread_id` is None forever —
    /// everything downstream hangs off it: no `Ready`, the queued first turn is not
    /// dispatched, the transcript has no tailer, and every later `turn.start` answers only
    /// "the thread is still opening". `session.start`/`session.resume` returned success long
    /// ago, and what ships is a dead session, idle forever, dragging a live
    /// `codex app-server` child, with nobody told.
    #[tokio::test]
    async fn a_native_refusal_of_the_handshake_ends_the_session_instead_of_hanging_it_forever() {
        // `thread/resume` after the rollout was cleaned up: the shape of a native refusal.
        let mut resuming = probe();
        resuming.resume_from = Some("thread-gone".into());
        assert!(
            resuming
                .classify(json!({"id": 1, "result": {}}))
                .await
                .is_none(),
            "the initialize response only opens the thread"
        );
        let (open_id, method) = resuming
            .handshake_request
            .expect("thread/resume must be tracked as the in-flight handshake request");
        assert_eq!(method, "thread/resume");
        let refused = resuming
            .classify(json!({
                "id": open_id,
                "error": {"code": -32600, "message": "no rollout found for thread id thread-gone"}
            }))
            .await;
        assert!(
            matches!(
                &refused,
                Some(HarnessEvent::ProtocolInvariant { message, .. })
                    if message.contains("thread/resume") && message.contains("no rollout found")
            ),
            "a rejected thread/resume must end the session, not become a Notice nobody reads: {refused:?}"
        );
        assert!(
            resuming.thread_id.is_none(),
            "a refused thread open cannot have bound a thread"
        );
        // This is the death nobody sees when it is degraded to a Notice: the session is
        // still there and a turn can never be opened.
        assert!(matches!(
            resuming.start_turn("anything", false, None).await,
            TurnStartDispatch::Resolved(TurnStartOutcome::RetryableNotAccepted { message })
                if message.contains("still opening")
        ));
        resuming
            .shutdown()
            .await
            .expect("stop resuming test process");

        // The first link of the same chain: a refused `initialize` leaves the thread just
        // as unopenable.
        let mut initializing = probe();
        initializing.handshake_request = Some((1, "initialize"));
        let refused = initializing
            .classify(json!({"id": 1, "error": {"message": "unsupported client capability"}}))
            .await;
        assert!(
            matches!(
                &refused,
                Some(HarnessEvent::ProtocolInvariant { message, .. })
                    if message.contains("initialize") && message.contains("unsupported client capability")
            ),
            "a rejected initialize must end the session too: {refused:?}"
        );
        assert!(
            !initializing.started,
            "a refused initialize must not leave the driver claiming an opened thread"
        );
        initializing
            .shutdown()
            .await
            .expect("stop initializing test process");
    }

    /// The two axes must move together. A mode that loosens the approval
    /// policy while leaving a read-only sandbox is not "more autonomous", it is
    /// an agent that fails without asking anyone — the worst of both.
    #[test]
    fn a_mode_moves_the_approval_policy_and_the_sandbox_together() {
        assert_eq!(
            native_policy(PermissionMode::Default),
            ("on-request", "workspace-write")
        );
        // Auto stops asking but stays inside the workspace.
        assert_eq!(
            native_policy(PermissionMode::Auto),
            ("never", "workspace-write")
        );
        // Plan is read-only *and* silent: asking about an edit it cannot make
        // would be a prompt with no good answer.
        assert_eq!(native_policy(PermissionMode::Plan), ("never", "read-only"));
        assert_eq!(
            native_policy(PermissionMode::Bypass),
            ("never", "danger-full-access")
        );
        // Never offered for codex, but must still be safe if it arrives.
        assert_eq!(
            native_policy(PermissionMode::AcceptEdits),
            native_policy(PermissionMode::Default),
            "an inexpressible mode falls back to asking, never to allowing"
        );
    }

    /// `thread/start` takes a string, `turn/start` takes a tagged object.
    /// Sending one where the other belongs is the easiest mistake in this file.
    #[test]
    fn the_turn_level_sandbox_uses_the_object_spelling() {
        assert_eq!(
            turn_sandbox_policy(PermissionMode::Plan),
            json!({"type": "readOnly"})
        );
        assert_eq!(
            turn_sandbox_policy(PermissionMode::Auto),
            json!({"type": "workspaceWrite"})
        );
        assert_eq!(
            turn_sandbox_policy(PermissionMode::Bypass),
            json!({"type": "dangerFullAccess"})
        );
    }

    /// codex cannot switch mid-turn, and the capability has to say so —
    /// otherwise the UI reports a guard as lifted while the running turn is
    /// still enforcing the old one.
    #[test]
    fn codex_reports_that_a_switch_waits_for_the_next_turn() {
        let c = capability();
        assert_eq!(c.permission_switch, Some(PermissionApply::NextTurn));
        assert!(
            !c.permission_modes.contains(&PermissionMode::AcceptEdits),
            "codex has no edits-vs-commands split; offering it would be a lie"
        );
    }

    #[test]
    fn codex_frames_parse_without_a_jsonrpc_field() {
        // Verbatim shape from `codex app-server`: no "jsonrpc" key anywhere.
        // A strict JSON-RPC 2.0 parser rejects this; ours must not.
        let notif: Value = serde_json::from_str(
            r#"{"method":"item/agentMessage/delta","params":{"threadId":"t1","turnId":"u1","itemId":"i1","delta":"hi"}}"#,
        )
        .unwrap();
        assert!(notif.get("jsonrpc").is_none());
        assert_eq!(notif["params"]["delta"], "hi");
    }

    #[test]
    fn thread_items_map_to_streaming_kinds() {
        let cmd = json!({"type":"commandExecution","id":"i","command":"cargo test"});
        assert_eq!(item_kind(&cmd).0, ItemKind::ToolCall);
        let msg = json!({"type":"agentMessage","id":"i","text":"hello"});
        assert_eq!(item_kind(&msg).0, ItemKind::AssistantMessage);
        let r = json!({"type":"reasoning","id":"i","summary":[],"content":[]});
        assert_eq!(item_kind(&r).0, ItemKind::Reasoning);
    }

    /// External RPCs never occupy the creation-time pre-ready slot or report a
    /// false empty-id success. No native bytes or pending mode are consumed.
    #[tokio::test]
    async fn an_external_pre_ready_turn_is_retryable_and_never_consumes_a_mode() {
        let mut driver = probe();
        driver
            .set_permission_mode(PermissionMode::Plan)
            .await
            .expect("queue the stricter mode");

        assert!(matches!(
            driver
                .start_turn("first", true, Some(guard_attempt(PermissionMode::Plan)))
                .await,
            TurnStartDispatch::Resolved(TurnStartOutcome::RetryableNotAccepted { message })
                if message.contains("still opening")
        ));
        assert_eq!(driver.pending_mode, Some(PermissionMode::Plan));
        assert!(driver.pending_turn_start.is_none());
        assert_eq!(driver.permission_mode(), PermissionMode::Default);

        driver.shutdown().await.expect("stop test process");
    }

    #[tokio::test]
    async fn a_second_start_during_a_live_turn_writes_nothing_and_retains_the_mode() {
        let mut driver = probe_with_responses(&[]);
        driver.current_turn = Some("existing-turn".into());
        driver.pending_mode = Some(PermissionMode::Plan);

        assert!(matches!(
            driver
                .start_turn(
                    "duplicate",
                    true,
                    Some(guard_attempt(PermissionMode::Plan)),
                )
                .await,
            TurnStartDispatch::Resolved(TurnStartOutcome::RetryableNotAccepted { message })
                if message.contains("already running")
        ));
        assert_eq!(driver.current_turn.as_deref(), Some("existing-turn"));
        assert_eq!(driver.pending_mode, Some(PermissionMode::Plan));
        assert!(driver.pending_turn_start.is_none());
        driver.shutdown().await.expect("stop test process");
    }

    #[tokio::test]
    async fn a_queued_mode_without_its_durable_attempt_is_refused_before_native_write() {
        let mut driver = probe_with_responses(&[]);
        driver.pending_mode = Some(PermissionMode::Plan);

        assert!(matches!(
            driver.start_turn("inspect", true, None).await,
            TurnStartDispatch::Resolved(TurnStartOutcome::RetryableNotAccepted { message })
                if message.contains("do not match")
        ));
        assert_eq!(driver.pending_mode, Some(PermissionMode::Plan));
        assert!(driver.pending_turn_start.is_none());
        assert!(driver.current_turn.is_none());
        driver.shutdown().await.expect("stop test process");
    }

    #[tokio::test]
    async fn a_durable_attempt_without_the_queued_mode_is_refused_before_native_write() {
        let mut driver = probe_with_responses(&[]);

        assert!(matches!(
            driver
                .start_turn("inspect", true, Some(guard_attempt(PermissionMode::Plan)))
                .await,
            TurnStartDispatch::Resolved(TurnStartOutcome::RetryableNotAccepted { message })
                if message.contains("cannot apply")
        ));
        assert_eq!(driver.pending_mode, None);
        assert!(driver.pending_turn_start.is_none());
        assert!(driver.current_turn.is_none());
        driver.shutdown().await.expect("stop test process");
    }

    /// A creation prompt is dispatched with `consume_pending_mode = false`, so
    /// a mode queued after session creation belongs to the following turn.
    #[tokio::test]
    async fn a_creation_turn_does_not_consume_a_later_mode() {
        let mut driver = probe_with_responses(&[json!({
            "id": 1,
            "result": {"turn": {"id": "turn-1"}}
        })]);
        driver
            .set_permission_mode(PermissionMode::Plan)
            .await
            .expect("queue mode after prompt");

        assert_eq!(
            driver.start_turn("creation prompt", false, None).await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnStartResolved(TurnStartOutcome::Accepted {
                turn_id,
                still_running: true,
                consumed_mode: None,
                ..
            })) if turn_id == "turn-1"
        ));

        assert_eq!(driver.current_turn.as_deref(), Some("turn-1"));
        assert_eq!(driver.permission_mode(), PermissionMode::Default);
        assert_eq!(
            driver.pending_mode,
            Some(PermissionMode::Plan),
            "the later mode was not stolen by the already-queued prompt"
        );
        driver.shutdown().await.expect("stop test process");
    }

    /// `turn/start` is not accepted when stdin is flushed. Interleaved native
    /// lines are emitted immediately and in order; only the exact response
    /// promotes the sticky mode.
    #[tokio::test]
    async fn turn_start_preserves_event_order_before_promoting_the_mode() {
        let mut driver = probe_with_responses(&[
            json!({
                "method": "item/agentMessage/delta",
                "params": {"itemId": "item-1", "delta": "still here"}
            }),
            json!({"id": 99, "result": {}}),
            json!({"id": 1, "result": {"turn": {"id": "turn-1"}}}),
        ]);
        driver.pending_mode = Some(PermissionMode::Plan);

        assert_eq!(
            driver
                .start_turn("inspect", true, Some(guard_attempt(PermissionMode::Plan)))
                .await,
            TurnStartDispatch::Awaiting
        );
        assert_eq!(driver.permission_mode(), PermissionMode::Default);
        assert_eq!(driver.pending_mode, None);
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::Delta { item_id, text })
                if item_id == "item-1" && text == "still here"
        ));
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnStartResolved(TurnStartOutcome::Accepted {
                turn_id,
                still_running: true,
                consumed_mode: Some(PermissionMode::Plan),
                ..
            })) if turn_id == "turn-1"
        ));
        assert_eq!(driver.permission_mode(), PermissionMode::Plan);
        driver.shutdown().await.expect("stop test process");
    }

    /// Notifications and client responses share stdout, so a sufficiently
    /// short turn may finish before Codex writes the exact `turn/start`
    /// response. Acceptance remains authoritative, but it cannot make the
    /// completed turn current again.
    #[tokio::test]
    async fn completion_only_evidence_can_accept_the_pending_turn() {
        let mut driver = probe_with_responses(&[
            json!({
                "method": "turn/completed",
                "params": {"turn": {"id": "turn-1", "status": "completed"}}
            }),
            json!({"id": 1, "result": {"turn": {"id": "turn-1"}}}),
        ]);
        assert_eq!(
            driver.start_turn("quick", true, None).await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnCompleted { turn_id, .. }) if turn_id == "turn-1"
        ));
        assert!(driver.current_turn.is_none());
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnStartResolved(TurnStartOutcome::Accepted {
                turn_id,
                still_running: false,
                ..
            })) if turn_id == "turn-1"
        ));
        assert!(driver.current_turn.is_none());
        driver.shutdown().await.expect("stop test process");
    }

    /// `turn/started` can likewise precede completion. This covers the native
    /// head plus tail ordering while the exact response is still outstanding.
    #[tokio::test]
    async fn completion_before_turn_response_does_not_resurrect_the_turn() {
        let mut driver = probe_with_responses(&[
            json!({
                "method": "turn/started",
                "params": {"turn": {"id": "turn-1"}}
            }),
            json!({
                "method": "turn/completed",
                "params": {"turn": {"id": "turn-1", "status": "completed"}}
            }),
            json!({"id": 1, "result": {"turn": {"id": "turn-1"}}}),
            json!({
                "method": "turn/started",
                "params": {"turn": {"id": "turn-1"}}
            }),
            json!({"id": 2, "result": {"turn": {"id": "turn-2"}}}),
        ]);

        assert_eq!(
            driver.start_turn("quick", true, None).await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnStarted { turn_id, .. }) if turn_id == "turn-1"
        ));
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnCompleted { turn_id, .. }) if turn_id == "turn-1"
        ));
        assert!(driver.current_turn.is_none());
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnStartResolved(TurnStartOutcome::Accepted {
                turn_id,
                still_running: false,
                consumed_mode: None,
                ..
            })) if turn_id == "turn-1"
        ));
        assert!(
            driver.current_turn.is_none(),
            "the late response must not resurrect a completed turn"
        );
        assert_eq!(
            driver.start_turn("next", true, None).await,
            TurnStartDispatch::Awaiting,
            "a late native start for the completed id cannot block the next turn"
        );
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnStartResolved(TurnStartOutcome::Accepted {
                turn_id,
                still_running: true,
                ..
            })) if turn_id == "turn-2"
        ));
        assert_eq!(driver.current_turn.as_deref(), Some("turn-2"));
        driver.shutdown().await.expect("stop test process");
    }

    #[tokio::test]
    async fn an_approval_after_completion_cannot_revive_the_turn() {
        let mut driver = probe();
        driver.current_turn = Some("turn-1".into());
        assert!(matches!(
            driver
                .classify(json!({
                    "method": "turn/completed",
                    "params": {"turn": {"id": "turn-1", "status": "completed"}}
                }))
                .await,
            Some(HarnessEvent::TurnCompleted { turn_id, .. }) if turn_id == "turn-1"
        ));
        assert!(
            driver
                .classify(json!({
                    "method": "turn/completed",
                    "params": {"turn": {"id": "turn-1", "status": "completed"}}
                }))
                .await
                .is_none(),
            "a duplicate completion is ignored before it can touch later state"
        );
        assert!(matches!(
            driver.classify_server_request(
                "item/commandExecution/requestApproval",
                &json!({
                    "approvalId": "late-approval",
                    "itemId": "item-1",
                    "turnId": "turn-1",
                    "command": "cargo test"
                }),
                json!(77),
            ),
            Some(HarnessEvent::ProtocolInvariant { message, .. })
                if message.contains("already completed")
        ));
        assert!(driver.current_turn.is_none());
        assert!(driver.pending_approvals.is_empty());
        driver.shutdown().await.expect("stop test process");
    }

    #[tokio::test]
    async fn every_codex_approval_requires_a_nonempty_string_turn_id_before_side_effects() {
        let methods = [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
            "item/permissions/requestApproval",
        ];
        let invalid_shapes = [None, Some(Value::Null), Some(json!("")), Some(json!(42))];

        for method in methods {
            for (index, turn_id) in invalid_shapes.iter().enumerate() {
                let mut driver = probe();
                let mut params = json!({
                    "approvalId": format!("approval-{index}"),
                    "itemId": "item-1",
                    "command": "cargo test"
                });
                if let Some(turn_id) = turn_id {
                    params["turnId"] = turn_id.clone();
                }
                assert!(matches!(
                    driver.classify_server_request(method, &params, json!(index)),
                    Some(HarnessEvent::ProtocolInvariant { message, .. })
                        if message.contains(method) && message.contains("turnId")
                ));
                assert!(driver.current_turn.is_none());
                assert!(driver.pending_turn_start.is_none());
                assert!(driver.pending_approvals.is_empty());
                driver.shutdown().await.expect("stop test process");
            }

            let mut driver = probe();
            assert!(matches!(
                driver.classify_server_request(
                    method,
                    &json!({
                        "approvalId": "valid",
                        "itemId": "item-1",
                        "turnId": "turn-1",
                        "command": "cargo test",
                        "permissions": {}
                    }),
                    json!(99),
                ),
                Some(HarnessEvent::Approval(ApprovalRequest { turn_id, .. }))
                    if turn_id == "turn-1"
            ));
            assert_eq!(driver.current_turn.as_deref(), Some("turn-1"));
            assert_eq!(driver.pending_approvals.len(), 1);
            driver.shutdown().await.expect("stop test process");
        }
    }

    #[tokio::test]
    async fn every_native_turn_identity_entry_accepts_256_bytes_and_rejects_257_before_state() {
        let max_id = "x".repeat(crate::rc::harness::MAX_NATIVE_TURN_ID_BYTES);
        let oversized = "x".repeat(crate::rc::harness::MAX_NATIVE_TURN_ID_BYTES + 1);

        let mut exact = probe_with_responses(&[json!({
            "id": 1,
            "result": {"turn": {"id": max_id}}
        })]);
        assert_eq!(
            exact.start_turn("exact", true, None).await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            exact.next_event().await,
            Some(HarnessEvent::TurnStartResolved(TurnStartOutcome::Accepted { turn_id, .. }))
                if turn_id.len() == crate::rc::harness::MAX_NATIVE_TURN_ID_BYTES
        ));
        exact.shutdown().await.expect("stop exact test process");

        let mut exact_oversized = probe_with_responses(&[json!({
            "id": 1,
            "result": {"turn": {"id": oversized}}
        })]);
        assert_eq!(
            exact_oversized.start_turn("exact", true, None).await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            exact_oversized.next_event().await,
            Some(HarnessEvent::ProtocolInvariant { message, .. })
                if message.contains("257 bytes")
        ));
        assert!(exact_oversized.current_turn.is_none());
        assert!(exact_oversized.pending_turn_start.is_some());
        exact_oversized
            .shutdown()
            .await
            .expect("stop oversized exact test process");

        let mut started = probe();
        assert!(matches!(
            started
                .classify(json!({
                    "method": "turn/started",
                    "params": {"turn": {"id": max_id}}
                }))
                .await,
            Some(HarnessEvent::TurnStarted { turn_id, .. })
                if turn_id.len() == crate::rc::harness::MAX_NATIVE_TURN_ID_BYTES
        ));
        let active = started.current_turn.clone();
        assert!(matches!(
            started
                .classify(json!({
                    "method": "turn/started",
                    "params": {"turn": {"id": oversized}}
                }))
                .await,
            Some(HarnessEvent::ProtocolInvariant { .. })
        ));
        assert_eq!(started.current_turn, active);
        started.shutdown().await.expect("stop started test process");

        let mut completed = probe();
        completed.current_turn = Some(max_id.clone());
        assert!(matches!(
            completed
                .classify(json!({
                    "method": "turn/completed",
                    "params": {"turn": {"id": max_id, "status": "completed"}}
                }))
                .await,
            Some(HarnessEvent::TurnCompleted { turn_id, .. })
                if turn_id.len() == crate::rc::harness::MAX_NATIVE_TURN_ID_BYTES
        ));
        completed.current_turn = Some("turn-live".into());
        assert!(matches!(
            completed
                .classify(json!({
                    "method": "turn/completed",
                    "params": {"turn": {"id": oversized, "status": "completed"}}
                }))
                .await,
            Some(HarnessEvent::ProtocolInvariant { .. })
        ));
        assert_eq!(completed.current_turn.as_deref(), Some("turn-live"));
        completed
            .shutdown()
            .await
            .expect("stop completion test process");

        let mut approval = probe();
        assert!(matches!(
            approval.classify_server_request(
                "item/commandExecution/requestApproval",
                &json!({"turnId": max_id, "approvalId": "ok", "itemId": "item"}),
                json!(1),
            ),
            Some(HarnessEvent::Approval(_))
        ));
        let pending = approval.pending_approvals.len();
        let current = approval.current_turn.clone();
        assert!(matches!(
            approval.classify_server_request(
                "item/commandExecution/requestApproval",
                &json!({"turnId": oversized, "approvalId": "bad", "itemId": "item"}),
                json!(2),
            ),
            Some(HarnessEvent::ProtocolInvariant { .. })
        ));
        assert_eq!(approval.pending_approvals.len(), pending);
        assert_eq!(approval.current_turn, current);
        approval
            .shutdown()
            .await
            .expect("stop approval test process");
    }

    #[tokio::test]
    async fn completion_budget_failure_preserves_the_entire_active_lifecycle() {
        let mut driver = probe();
        driver.completed_turn_ids = BoundedTurnIds::with_limits(1, 64);
        driver
            .remember_completed_turn("turn-old".into())
            .expect("fill the single tombstone slot");
        driver.current_turn = Some("turn-live".into());
        driver.pending_approvals.insert(
            "approval-live".into(),
            PendingApproval {
                req_id: json!(5),
                method: "item/commandExecution/requestApproval".into(),
                requested_permissions: None,
            },
        );

        assert!(matches!(
            driver
                .classify(json!({
                    "method": "turn/completed",
                    "params": {"turn": {"id": "turn-live", "status": "completed"}}
                }))
                .await,
            Some(HarnessEvent::ProtocolInvariant { message, .. })
                if message.contains("budget")
        ));
        assert_eq!(driver.current_turn.as_deref(), Some("turn-live"));
        assert!(driver.pending_approvals.contains_key("approval-live"));
        assert!(!driver.completed_turn("turn-live"));
        assert_eq!(driver.completed_turn_ids.len(), 1);
        driver.shutdown().await.expect("stop test process");
    }

    #[tokio::test]
    async fn exact_acceptance_fences_later_started_and_approval_turn_ids() {
        let mut driver = probe_with_responses(&[json!({
            "id": 1,
            "result": {"turn": {"id": "turn-a"}}
        })]);
        assert_eq!(
            driver.start_turn("inspect", true, None).await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnStartResolved(TurnStartOutcome::Accepted {
                turn_id,
                ..
            })) if turn_id == "turn-a"
        ));
        assert_eq!(driver.current_turn.as_deref(), Some("turn-a"));

        assert!(matches!(
            driver
                .classify(json!({
                    "method": "turn/started",
                    "params": {"turn": {"id": "turn-b"}}
                }))
                .await,
            Some(HarnessEvent::ProtocolInvariant { message, .. })
                if message.contains("active turn turn-a")
        ));
        assert_eq!(
            driver.current_turn.as_deref(),
            Some("turn-a"),
            "conflicting started evidence must not overwrite the accepted id"
        );

        driver
            .remember_completed_turn("turn-old".into())
            .expect("test identity fits");
        assert!(
            driver
                .classify(json!({
                    "method": "turn/started",
                    "params": {"turn": {"id": "turn-old"}}
                }))
                .await
                .is_none()
        );
        assert_eq!(
            driver.current_turn.as_deref(),
            Some("turn-a"),
            "a passive duplicate for old turn A cannot perturb live turn B"
        );

        assert!(matches!(
            driver.classify_server_request(
                "item/commandExecution/requestApproval",
                &json!({
                    "approvalId": "wrong-turn",
                    "itemId": "item-b",
                    "turnId": "turn-b",
                    "command": "cargo test"
                }),
                json!(77),
            ),
            Some(HarnessEvent::ProtocolInvariant { message, .. })
                if message.contains("active turn turn-a")
        ));
        assert!(
            driver.pending_approvals.is_empty(),
            "a conflicting approval must not leave an answerable card"
        );
        assert_eq!(driver.current_turn.as_deref(), Some("turn-a"));

        assert!(
            driver
                .classify(json!({
                    "method": "turn/started",
                    "params": {"turn": {"id": "turn-a"}}
                }))
                .await
                .is_none(),
            "matching started evidence is a duplicate, not a second head"
        );
        assert!(matches!(
            driver.classify_server_request(
                "item/commandExecution/requestApproval",
                &json!({
                    "approvalId": "same-turn",
                    "itemId": "item-a",
                    "turnId": "turn-a",
                    "command": "cargo test"
                }),
                json!(78),
            ),
            Some(HarnessEvent::Approval(ApprovalRequest { approval_id, .. }))
                if approval_id == "same-turn"
        ));
        assert_eq!(driver.current_turn.as_deref(), Some("turn-a"));
        driver.shutdown().await.expect("stop test process");
    }

    #[tokio::test]
    async fn invalid_completion_ids_never_advance_or_clear_the_active_lifecycle() {
        let mut driver = probe_with_responses(&[json!({
            "id": 1,
            "result": {"turn": {"id": "turn-a"}}
        })]);
        assert_eq!(
            driver.start_turn("inspect", true, None).await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnStartResolved(
                TurnStartOutcome::Accepted { .. }
            ))
        ));
        assert!(matches!(
            driver.classify_server_request(
                "item/commandExecution/requestApproval",
                &json!({
                    "approvalId": "approval-a",
                    "itemId": "item-a",
                    "turnId": "turn-a",
                    "command": "cargo test"
                }),
                json!(77),
            ),
            Some(HarnessEvent::Approval(_))
        ));

        for invalid in [
            json!({
                "method": "turn/completed",
                "params": {"turn": {"id": "turn-b", "status": "completed"}}
            }),
            json!({
                "method": "turn/completed",
                "params": {"turn": {"status": "completed"}}
            }),
            json!({
                "method": "turn/completed",
                "params": {"turn": {"id": "", "status": "completed"}}
            }),
        ] {
            assert!(matches!(
                driver.classify(invalid).await,
                Some(HarnessEvent::ProtocolInvariant { .. })
            ));
            assert_eq!(driver.current_turn.as_deref(), Some("turn-a"));
            assert!(driver.pending_approvals.contains_key("approval-a"));
            assert!(driver.completed_turn_ids.is_empty());
        }

        driver
            .remember_completed_turn("turn-old".into())
            .expect("test identity fits");
        assert!(
            driver
                .classify(json!({
                    "method": "turn/completed",
                    "params": {"turn": {"id": "turn-old", "status": "completed"}}
                }))
                .await
                .is_none()
        );
        assert_eq!(driver.current_turn.as_deref(), Some("turn-a"));
        assert!(driver.pending_approvals.contains_key("approval-a"));

        assert!(matches!(
            driver
                .classify(json!({
                    "method": "turn/completed",
                    "params": {"turn": {"id": "turn-a", "status": "completed"}}
                }))
                .await,
            Some(HarnessEvent::TurnCompleted { turn_id, .. }) if turn_id == "turn-a"
        ));
        assert!(driver.current_turn.is_none());
        assert!(
            driver
                .classify(json!({
                    "method": "turn/completed",
                    "params": {"turn": {"id": "turn-a", "status": "completed"}}
                }))
                .await
                .is_none(),
            "a matching duplicate completion stays silent"
        );
        driver.shutdown().await.expect("stop test process");
    }

    #[tokio::test]
    async fn completed_turn_identities_outlive_the_former_lru_limit() {
        const FORMER_COMPLETED_TURN_LIMIT: usize = 32;
        let mut driver = probe();
        for index in 0..=FORMER_COMPLETED_TURN_LIMIT {
            let turn_id = format!("turn-{index}");
            driver.current_turn = Some(turn_id.clone());
            assert!(matches!(
                driver
                    .classify(json!({
                        "method": "turn/completed",
                        "params": {"turn": {"id": turn_id, "status": "completed"}}
                    }))
                    .await,
                Some(HarnessEvent::TurnCompleted { .. })
            ));
        }
        assert_eq!(
            driver.completed_turn_ids.len(),
            FORMER_COMPLETED_TURN_LIMIT + 1
        );
        assert!(
            driver
                .classify(json!({
                    "method": "turn/started",
                    "params": {"turn": {"id": "turn-0"}}
                }))
                .await
                .is_none(),
            "a late start cannot revive the first completed id"
        );
        assert!(matches!(
            driver.classify_server_request(
                "item/commandExecution/requestApproval",
                &json!({
                    "approvalId": "late",
                    "itemId": "late-item",
                    "turnId": "turn-0",
                    "command": "cargo test"
                }),
                json!(99),
            ),
            Some(HarnessEvent::ProtocolInvariant { .. })
        ));
        assert!(driver.pending_approvals.is_empty());
        assert!(
            driver
                .classify(json!({
                    "method": "turn/completed",
                    "params": {"turn": {"id": "turn-0", "status": "completed"}}
                }))
                .await
                .is_none(),
            "a late duplicate completion cannot settle twice"
        );
        assert!(driver.current_turn.is_none());
        driver.shutdown().await.expect("stop test process");
    }

    #[tokio::test]
    async fn retired_start_correlations_outlive_the_former_lru_limit() {
        const FORMER_RETIRED_TURN_START_LIMIT: usize = 16;
        let mut driver = probe();
        for request_id in 1..=(FORMER_RETIRED_TURN_START_LIMIT as i64 + 1) {
            let turn_id = format!("turn-{request_id}");
            let token = format!("guard-{request_id}");
            assert!(matches!(
                driver.outcome_after_turn_response_loss(
                    PendingTurnStart {
                        request_id,
                        staged_mode: Some(PermissionMode::Plan),
                        guard_attempt: Some(TurnGuardAttempt {
                            token,
                            expected_mode: PermissionMode::Plan,
                        }),
                        deadline: tokio::time::Instant::now(),
                        observed_turn_id: Some(turn_id),
                        completed: true,
                    },
                    "deadline".into(),
                ),
                Ok(TurnStartOutcome::Accepted {
                    still_running: false,
                    ..
                })
            ));
        }
        assert_eq!(
            driver.retired_turn_starts.len(),
            FORMER_RETIRED_TURN_START_LIMIT + 1
        );
        assert!(matches!(
            driver
                .classify(json!({"id": 1, "result": {"turn": {"id": "turn-1"}}}))
                .await,
            Some(HarnessEvent::TurnStartConfirmed {
                confirmation_token: Some(token),
                ..
            }) if token == "guard-1"
        ));
        assert!(matches!(
            driver
                .classify(json!({"id": 2, "error": {"message": "late refusal"}}))
                .await,
            Some(HarnessEvent::ProtocolInvariant {
                confirmation_token: Some(token),
                ..
            }) if token == "guard-2"
        ));
        assert_eq!(
            driver.retired_turn_starts.len(),
            FORMER_RETIRED_TURN_START_LIMIT - 1
        );
        driver.shutdown().await.expect("stop test process");
    }

    #[test]
    fn retired_start_budget_rejects_new_or_duplicate_entries_and_take_reclaims_bytes() {
        fn retired(turn_id: &str, token: &str) -> RetiredTurnStart {
            RetiredTurnStart {
                turn_id: turn_id.into(),
                confirmation_token: Some(token.into()),
                consumed_mode: Some(PermissionMode::Plan),
            }
        }

        let mut starts = RetiredTurnStarts::with_limits(1, 8);
        starts
            .try_insert(7, retired("turn", "tok"))
            .expect("first correlation fits");
        assert_eq!(starts.payload_bytes, 7);
        assert!(
            starts
                .try_insert(7, retired("other", "x"))
                .unwrap_err()
                .contains("reused")
        );
        assert!(
            starts
                .try_insert(8, retired("x", "y"))
                .unwrap_err()
                .contains("1 entries")
        );
        let taken = starts.take(7).expect("correlation remains intact");
        assert_eq!(taken.turn_id, "turn");
        assert_eq!(starts.payload_bytes, 0);
        starts
            .try_insert(8, retired("next", "tok"))
            .expect("take releases both entry and payload budget");

        let mut payload_limited = RetiredTurnStarts::with_limits(4, 3);
        assert!(
            payload_limited
                .try_insert(1, retired("ab", "cd"))
                .unwrap_err()
                .contains("payload")
        );
        assert!(payload_limited.entries.is_empty());
        assert_eq!(payload_limited.payload_bytes, 0);
    }

    #[tokio::test]
    async fn retired_budget_failure_does_not_apply_mode_or_live_turn_state() {
        let mut driver = probe();
        driver.retired_turn_starts = RetiredTurnStarts::with_limits(0, 0);
        let event = driver
            .outcome_after_turn_response_loss(
                PendingTurnStart {
                    request_id: 1,
                    staged_mode: Some(PermissionMode::Bypass),
                    guard_attempt: Some(TurnGuardAttempt {
                        token: "guard".into(),
                        expected_mode: PermissionMode::Bypass,
                    }),
                    deadline: tokio::time::Instant::now(),
                    observed_turn_id: Some("turn-1".into()),
                    completed: false,
                },
                "deadline".into(),
            )
            .expect_err("a zero-capacity correlation ledger fails closed");
        assert!(matches!(
            event,
            RetiredTurnStartError {
                attempted_mode: Some(PermissionMode::Bypass),
                confirmation_token: Some(token),
                ..
            } if token == "guard"
        ));
        assert_eq!(driver.permission_mode(), PermissionMode::Default);
        assert!(driver.current_turn.is_none());
        assert!(driver.retired_turn_starts.is_empty());
        driver.shutdown().await.expect("stop test process");
    }

    /// `turn/started` itself proves acceptance. Losing the later client
    /// response must not turn that proven start into Unknown and kill a live
    /// session merely because stdout interleaving delayed the receipt.
    #[tokio::test]
    async fn a_started_notification_survives_turn_response_timeout() {
        let mut driver = probe_with_responses(&[]);
        driver.pending_mode = Some(PermissionMode::Plan);

        assert_eq!(
            driver
                .start_turn("inspect", true, Some(guard_attempt(PermissionMode::Plan)))
                .await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            driver
                .classify(json!({
                    "method": "turn/started",
                    "params": {"turn": {"id": "turn-1"}}
                }))
                .await,
            Some(HarnessEvent::TurnStarted { turn_id, .. }) if turn_id == "turn-1"
        ));
        let pending = driver.pending_turn_start.take().unwrap();
        let outcome = driver.outcome_after_turn_response_loss(
            pending,
            "codex did not confirm turn/start within 10s".into(),
        );
        assert!(matches!(
            outcome,
            Ok(TurnStartOutcome::Accepted {
                turn_id,
                still_running: true,
                consumed_mode: Some(PermissionMode::Plan),
                ..
            }) if turn_id == "turn-1"
        ));
        assert_eq!(driver.current_turn.as_deref(), Some("turn-1"));
        assert_eq!(driver.permission_mode(), PermissionMode::Plan);
        driver.shutdown().await.expect("stop test process");
    }

    #[tokio::test]
    async fn a_late_exact_response_after_interrupted_completion_cannot_resurrect_the_turn() {
        let mut driver = probe_with_responses(&[]);
        assert_eq!(
            driver.start_turn("quick", true, None).await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            driver
                .classify(json!({
                    "method": "turn/started",
                    "params": {"turn": {"id": "turn-1"}}
                }))
                .await,
            Some(HarnessEvent::TurnStarted { .. })
        ));
        assert!(matches!(
            driver
                .classify(json!({
                    "method": "turn/completed",
                    "params": {"turn": {"id": "turn-1", "status": "interrupted"}}
                }))
                .await,
            Some(HarnessEvent::TurnCompleted {
                outcome: TurnOutcome::Interrupted,
                ..
            })
        ));
        let pending = driver.pending_turn_start.take().unwrap();
        assert!(matches!(
            driver.outcome_after_turn_response_loss(pending, "deadline".into()),
            Ok(TurnStartOutcome::Accepted {
                still_running: false,
                ..
            })
        ));
        assert!(driver.current_turn.is_none());

        assert!(
            driver
                .classify(json!({"id": 1, "result": {"turn": {"id": "turn-1"}}}))
                .await
                .is_none(),
            "the consistent late response is consumed without a new event"
        );
        assert!(
            driver.current_turn.is_none(),
            "generic result handling must not revive the completed turn"
        );
        assert!(driver.retired_turn_starts.is_empty());
        driver.shutdown().await.expect("stop test process");
    }

    #[tokio::test]
    async fn a_conflicting_late_turn_response_is_a_fatal_protocol_invariant() {
        let mut driver = probe_with_responses(&[]);
        assert_eq!(
            driver.start_turn("inspect", true, None).await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            driver
                .classify(json!({
                    "method": "turn/started",
                    "params": {"turn": {"id": "turn-1"}}
                }))
                .await,
            Some(HarnessEvent::TurnStarted { .. })
        ));
        let pending = driver.pending_turn_start.take().unwrap();
        assert!(matches!(
            driver.outcome_after_turn_response_loss(pending, "deadline".into()),
            Ok(TurnStartOutcome::Accepted { .. })
        ));

        assert!(matches!(
            driver
                .classify(json!({"id": 1, "result": {"turn": {"id": "other-turn"}}}))
                .await,
            Some(HarnessEvent::ProtocolInvariant { message, .. })
                if message.contains("contradicted accepted turn turn-1")
        ));
        driver.shutdown().await.expect("stop test process");
    }

    #[tokio::test]
    async fn a_retired_contradiction_keeps_its_own_guard_identity_during_a_new_start() {
        let mut driver = probe_with_responses(&[]);
        driver.pending_mode = Some(PermissionMode::Plan);
        let old_guard = TurnGuardAttempt {
            token: "old-guard".into(),
            expected_mode: PermissionMode::Plan,
        };
        assert_eq!(
            driver
                .start_turn("old", true, Some(old_guard.clone()))
                .await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            driver
                .classify(json!({
                    "method": "turn/started",
                    "params": {"turn": {"id": "old-turn"}}
                }))
                .await,
            Some(HarnessEvent::TurnStarted { .. })
        ));
        assert!(matches!(
            driver
                .classify(json!({
                    "method": "turn/completed",
                    "params": {"turn": {"id": "old-turn", "status": "completed"}}
                }))
                .await,
            Some(HarnessEvent::TurnCompleted { .. })
        ));
        let pending = driver.pending_turn_start.take().unwrap();
        assert!(matches!(
            driver.outcome_after_turn_response_loss(pending, "deadline".into()),
            Ok(TurnStartOutcome::Accepted {
                confirmation: TurnStartConfirmation::NotificationOnly,
                ..
            })
        ));

        driver.pending_mode = Some(PermissionMode::Auto);
        let new_guard = TurnGuardAttempt {
            token: "new-guard".into(),
            expected_mode: PermissionMode::Auto,
        };
        assert_eq!(
            driver
                .start_turn("new", true, Some(new_guard.clone()))
                .await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            driver
                .classify(json!({"id": 1, "result": {"turn": {"id": "wrong-old"}}}))
                .await,
            Some(HarnessEvent::ProtocolInvariant {
                attempted_mode: None,
                confirmation_token: Some(token),
                ..
            }) if token == old_guard.token
        ));
        let pending = driver
            .pending_turn_start
            .as_ref()
            .expect("the unrelated newer request remains identifiable");
        assert_eq!(pending.staged_mode, Some(PermissionMode::Auto));
        assert_eq!(pending.guard_attempt.as_ref(), Some(&new_guard));
        driver.shutdown().await.expect("stop test process");
    }

    #[tokio::test]
    async fn an_approval_turn_id_proves_acceptance_past_the_start_deadline() {
        let mut driver = probe_with_responses(&[]);
        driver.pending_mode = Some(PermissionMode::Plan);
        assert_eq!(
            driver
                .start_turn("inspect", true, Some(guard_attempt(PermissionMode::Plan)))
                .await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            driver.classify_server_request(
                "item/commandExecution/requestApproval",
                &json!({
                    "approvalId": "approval-1",
                    "itemId": "item-1",
                    "turnId": "turn-1",
                    "command": "cargo test"
                }),
                json!(77),
            ),
            Some(HarnessEvent::Approval(ApprovalRequest { turn_id, .. }))
                if turn_id == "turn-1"
        ));
        assert_eq!(driver.current_turn.as_deref(), Some("turn-1"));

        // Model the response deadline expiring while a human is considering
        // the approval. The approval's turnId is already authoritative proof,
        // so the start resolves Accepted instead of killing the session.
        driver
            .pending_turn_start
            .as_mut()
            .expect("turn/start remains pending")
            .deadline = tokio::time::Instant::now();
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnStartResolved(
                TurnStartOutcome::Accepted {
                    turn_id,
                    still_running: true,
                    consumed_mode: Some(PermissionMode::Plan),
                    ..
                }
            )) if turn_id == "turn-1"
        ));
        assert_eq!(driver.permission_mode(), PermissionMode::Plan);
        driver.shutdown().await.expect("stop test process");
    }

    /// A server request that precedes the exact response remains answerable and
    /// reaches the supervisor before turn resolution.
    #[tokio::test]
    async fn an_approval_before_the_turn_response_is_emitted_first() {
        let mut driver = probe_with_responses(&[
            json!({
                "id": 77,
                "method": "item/commandExecution/requestApproval",
                "params": {
                    "approvalId": "approval-1",
                    "itemId": "item-1",
                    "turnId": "turn-1",
                    "command": "cargo test"
                }
            }),
            json!({"id": 1, "result": {"turn": {"id": "turn-1"}}}),
        ]);
        assert_eq!(
            driver.start_turn("inspect", true, None).await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::Approval(ApprovalRequest { approval_id, .. }))
                if approval_id == "approval-1"
        ));
        assert!(driver.pending_approvals.contains_key("approval-1"));
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnStartResolved(
                TurnStartOutcome::Accepted { .. }
            ))
        ));
        driver.shutdown().await.expect("stop test process");
    }

    /// A JSON-RPC error proves the native turn never accepted its sticky mode.
    /// Keep the old effective mode and restore the queued intent for retry.
    #[tokio::test]
    async fn an_explicit_turn_rejection_restores_the_staged_mode() {
        let mut driver = probe_with_responses(&[json!({
            "id": 1,
            "error": {"code": -32602, "message": "turn rejected"}
        })]);
        driver.mode = PermissionMode::Auto;
        driver.pending_mode = Some(PermissionMode::Plan);

        assert_eq!(
            driver
                .start_turn("inspect", true, Some(guard_attempt(PermissionMode::Plan)))
                .await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnStartResolved(TurnStartOutcome::ExplicitRefusal {
                message,
                retained_mode: Some(PermissionMode::Plan),
            })) if message.contains("turn rejected")
        ));
        assert_eq!(driver.permission_mode(), PermissionMode::Auto);
        assert_eq!(driver.pending_mode, Some(PermissionMode::Plan));
        assert!(driver.current_turn.is_none());
        driver.shutdown().await.expect("stop test process");
    }

    /// Restoration is conditional: an older refusal must never overwrite a
    /// newer queued authorization decision.
    #[tokio::test]
    async fn a_newer_queued_mode_supersedes_the_rejected_one() {
        let mut driver = probe_with_responses(&[json!({
            "id": 1,
            "error": {"code": -32602, "message": "turn rejected"}
        })]);
        driver.pending_mode = Some(PermissionMode::Plan);
        assert_eq!(
            driver
                .start_turn("inspect", true, Some(guard_attempt(PermissionMode::Plan)))
                .await,
            TurnStartDispatch::Awaiting
        );
        driver.pending_mode = Some(PermissionMode::Auto);
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnStartResolved(
                TurnStartOutcome::ExplicitRefusal {
                    retained_mode: Some(PermissionMode::Auto),
                    ..
                }
            ))
        ));
        assert_eq!(driver.pending_mode, Some(PermissionMode::Auto));
        driver.shutdown().await.expect("stop test process");
    }

    /// A malformed success does not prove either acceptance or rejection. It
    /// must not move the visible mode. The staged intent is deliberately not
    /// restored: retrying could duplicate an already-accepted prompt, so the
    /// supervisor will terminate this harness and persist a Plan restart.
    #[tokio::test]
    async fn a_malformed_turn_response_never_claims_the_mode_applied() {
        let mut driver = probe_with_responses(&[json!({"id": 1, "result": {}})]);
        driver.pending_mode = Some(PermissionMode::Bypass);

        assert_eq!(
            driver
                .start_turn("inspect", true, Some(guard_attempt(PermissionMode::Bypass)),)
                .await,
            TurnStartDispatch::Awaiting
        );
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::TurnStartResolved(TurnStartOutcome::Unknown {
                attempted_mode: Some(PermissionMode::Bypass),
                ..
            }))
        ));
        assert_eq!(driver.permission_mode(), PermissionMode::Default);
        assert_eq!(driver.pending_mode, None);
        driver.shutdown().await.expect("stop test process");
    }

    /// Server-request classification owns the JSON-RPC id needed for the
    /// eventual response. A turn boundary must expire that native half too.
    #[tokio::test]
    async fn abandoned_approvals_cannot_be_answered_after_a_turn_boundary() {
        let mut driver = probe();
        let event = driver
            .classify_server_request(
                "item/commandExecution/requestApproval",
                &json!({
                    "approvalId": "approval-1",
                    "itemId": "item-1",
                    "turnId": "turn-1",
                    "command": "git status"
                }),
                json!(41),
            )
            .expect("the production classifier should emit an approval");
        assert!(matches!(
            event,
            HarnessEvent::Approval(ApprovalRequest {
                can_allow_for_session: false,
                suggested_permission_mode: None,
                ..
            })
        ));
        assert_eq!(driver.pending_approvals.len(), 1);

        assert_eq!(driver.abandon_pending_approvals(), 1);
        assert!(driver.pending_approvals.is_empty());
        let stale = ApprovalResponse {
            approval_id: "approval-1".into(),
            session_id: "session-1".into(),
            decision: ApprovalDecision::Allow,
            scope: ApprovalScope::Once,
            message: None,
            by: Some("owner".into()),
        };
        assert!(matches!(
            driver.answer_approval(&stale).await,
            ApprovalOutcome::ExplicitRefusal {
                message,
                retained: false,
            } if message.contains("no longer pending")
        ));
        driver.shutdown().await.expect("stop test process");
    }

    /// An outright native refusal of `turn/steer` has to come back as an RPC error; a
    /// successful write alone must not be reported as "delivered" — otherwise the user
    /// believes the course changed while the agent never received that sentence.
    #[tokio::test]
    async fn a_native_steer_refusal_is_not_reported_as_delivered() {
        let mut driver = probe_with_responses(&[
            json!({"method": "item/started", "params": {"item": {"id": "item-1", "type": "agentMessage"}}}),
            json!({"id": 1, "error": {"message": "expected turn turn-1 is no longer running"}}),
        ]);
        driver.current_turn = Some("turn-1".into());

        let error = driver.steer("change course").await.unwrap_err();
        assert!(
            error.to_string().contains("codex refused turn/steer"),
            "expected a typed refusal, got: {error}"
        );
        // A notification read while waiting for the response cannot be dropped: the next
        // next_event has to release it first.
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::ItemStarted { item_id, .. }) if item_id == "item-1"
        ));
        driver.shutdown().await.expect("stop test process");
    }

    /// The contrast case: steer reports `Delivery::Immediate` only once the response confirms it.
    #[tokio::test]
    async fn a_confirmed_steer_reports_immediate_delivery() {
        let mut driver = probe_with_responses(&[json!({"id": 1, "result": {}})]);
        driver.current_turn = Some("turn-1".into());
        assert_eq!(
            driver
                .steer("change course")
                .await
                .expect("confirmed steer"),
            Delivery::Immediate
        );
        driver.shutdown().await.expect("stop test process");
    }

    /// Waiting for the steer receipt must not turn the pipe's bounded queue into an unbounded
    /// side channel. Notifications read ahead of the receipt remain charged to the shared byte
    /// budget until `next_event` replays (or otherwise drops) them.
    #[tokio::test]
    async fn steer_pushback_keeps_the_pipe_byte_budget_until_replayed() {
        let held = json!({
            "method": "item/agentMessage/delta",
            "params": {
                "threadId": "thread-1",
                "turnId": "turn-1",
                "itemId": "item-1",
                "delta": "held"
            }
        });
        let held_bytes = held.to_string().len();
        let mut driver = probe_with_responses(&[held, json!({"id": 1, "result": {}})]);
        driver.current_turn = Some("turn-1".into());
        let before = driver.proc.available_pending_bytes();

        assert_eq!(
            driver
                .steer("change course")
                .await
                .expect("confirmed steer"),
            Delivery::Immediate
        );
        assert_eq!(driver.pushback.len(), 1);
        assert_eq!(
            before - driver.proc.available_pending_bytes(),
            held_bytes.max(1),
            "a line moved into driver pushback must still consume the pipe byte budget"
        );

        drop(driver.pushback.pop_front());
        assert_eq!(
            driver.proc.available_pending_bytes(),
            before,
            "dropping the retained line must return its byte permit"
        );
        driver.shutdown().await.expect("stop test process");
    }

    /// **Lines held while waiting for the `turn/steer` response must not exhaust the
    /// shared byte budget.**
    ///
    /// This wait loop is itself stdout's only reader. A held line still holds part of the
    /// budget the two pipes share (which is what the case above pins), so once the hold is
    /// full the reader task parks on `queue_line`'s `acquire_many_owned` and **the response
    /// codex has already sent can never be read**: [`STEER_RESPONSE_TIMEOUT`] runs out and
    /// the call ends in "delivery unknown", so the supervisor treats a steer that did land
    /// as an unknown outcome and the whole harness process tree is killed.
    ///
    /// So a line past the cap is dropped on the spot and **its bytes are returned at
    /// once**; how many were dropped has to leave a trace, or afterwards nobody can tell
    /// "the model said nothing" from "we lost it".
    #[tokio::test]
    async fn a_steer_flood_past_the_pushback_cap_returns_its_bytes_and_leaves_a_notice() {
        let noise = |seq: usize| {
            json!({
                "method": "item/agentMessage/delta",
                "params": {
                    "threadId": "thread-1",
                    "turnId": "turn-1",
                    "itemId": "item-1",
                    "delta": format!("noise-{seq}")
                }
            })
        };
        let first_bytes = noise(1).to_string().len();
        let mut driver =
            probe_with_responses(&[noise(1), noise(2), noise(3), json!({"id": 1, "result": {}})]);
        driver.current_turn = Some("turn-1".into());
        // The real cap is `PUSHBACK_MAX_BYTES`; scaled down proportionally to its
        // minimum, so the same branch is reached without actually filling
        // `MAX_PENDING_BYTES`. The first line is always kept (the `bytes == 0` exception in
        // `Pushback::push`), the rest are over the cap.
        driver.pushback = Pushback::with_cap(1);
        let before = driver.proc.available_pending_bytes();

        assert_eq!(
            driver
                .steer("change course")
                .await
                .expect("the receipt must still reach us across the flood"),
            Delivery::Immediate
        );
        assert_eq!(
            before - driver.proc.available_pending_bytes(),
            first_bytes,
            "pushback kept the over-cap lines' byte permits — the reader task stays blocked and \
             the receipt we are waiting on can never be read"
        );

        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::Delta { text, .. }) if text == "noise-1"
        ));
        assert!(
            matches!(
                driver.next_event().await,
                Some(HarnessEvent::Notice { text }) if text.contains("dropped 2 harness output line")
            ),
            "the dropped stream lines vanished from the transcript without a trace"
        );
        driver.shutdown().await.expect("stop test process");
    }

    /// Stop pressed in the window where `turn/start` is written and its response has not
    /// come back: the turn is most likely already running natively, but there is no turn id
    /// yet to fill `turn/interrupt`. Returning Ok reports "dropped" as "stopped", so this
    /// must error and let the caller retry.
    #[tokio::test]
    async fn an_interrupt_during_turn_acceptance_is_not_silently_dropped() {
        let mut driver = probe();
        driver.set_test_thread_id("thread-1");
        driver.pending_turn_start = Some(PendingTurnStart {
            request_id: 7,
            staged_mode: None,
            guard_attempt: None,
            deadline: tokio::time::Instant::now() + TURN_START_RESPONSE_TIMEOUT,
            observed_turn_id: None,
            completed: false,
        });

        let error = driver.interrupt().await.unwrap_err();
        assert!(
            error.to_string().contains("awaiting native acceptance"),
            "expected a retryable error, got: {error}"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), driver.proc.next())
                .await
                .is_err(),
            "no native interrupt can be addressed without a turn id"
        );

        // Once the authoritative `turn/completed` has arrived there really is nothing to
        // stop — the no-op stands.
        driver
            .pending_turn_start
            .as_mut()
            .expect("pending turn seeded above")
            .completed = true;
        driver
            .interrupt()
            .await
            .expect("a completed pending turn leaves nothing to interrupt");
        driver.shutdown().await.expect("stop test process");
    }

    #[test]
    fn steer_is_immediate_here_unlike_claude_code() {
        assert_eq!(capability().steer, Some(Delivery::Immediate));
        assert_eq!(
            super::super::claude_code::capability().steer,
            Some(Delivery::AtToolBoundary)
        );
    }
}
