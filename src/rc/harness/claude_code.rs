//! claude-code driver — `stream-json` over stdio.
//!
//! # The exact invocation, and why each flag is there
//!
//! ```text
//! claude -p --input-format stream-json --output-format stream-json --verbose
//!        --include-partial-messages      token deltas, else the page only updates per block
//!        --replay-user-messages          our own messages come back on the stream, so every
//!                                        viewer sees what the driver typed (multi-viewer needs this)
//!        --permission-prompt-tool stdio  MEASURED: without it the CLI applies local settings and
//!                                        never asks. With it, every tool call arrives as a
//!                                        `can_use_tool` control request carrying full input.
//!        --permission-mode default       never `bypassPermissions` unless the caller asked
//!        --session-id <uuid>             we choose the id, so the transcript path is known up front
//!        [--resume <uuid>]               continue an existing session
//! ```
//!
//! # Measured behaviour (this is the part you cannot read off a doc page)
//!
//! Against `claude` 2.1.233 on this machine:
//!
//! * A `can_use_tool` request looks like
//!   `{"type":"control_request","request_id":"…","request":{"subtype":"can_use_tool","tool_name":"Write","input":{…},"permission_suggestions":[…],"tool_use_id":"toolu_…"}}`
//!   and the answer is
//!   `{"type":"control_response","response":{"subtype":"success","request_id":"…","response":{"behavior":"deny","message":"…"}}}`.
//!   A denial with a message is delivered to the model as an `is_error` tool
//!   result, and it explains itself in the next turn rather than silently retrying.
//! * `{"type":"control_request","request":{"subtype":"interrupt"}}` genuinely
//!   aborts an in-flight `sleep 30`; the transcript gets a
//!   `[Request interrupted by user]` user line and the process exits non-zero.
//! * A second user message written mid-turn is **accepted immediately but
//!   delivered only when the running tool call returns** (measured: written at
//!   4.00 s during a `sleep 30`, appeared in the stream at 35.06 s, and counted
//!   into the *same* turn — `num_turns=2`, no new turn). Hence
//!   [`Delivery::AtToolBoundary`]: the UI must show "queued", or on a weak
//!   network the user assumes it was lost and sends it three more times.

use super::proc::{LaunchError, Line, Proc, Pushback};
use super::{
    ApprovalOutcome, HarnessEvent, LaunchSpec, PermissionModeChangeError,
    PermissionModeChangeResult, TurnOutcome,
};
use crate::protocol::{
    ApprovalDecision, ApprovalKind, ApprovalRequest, ApprovalResponse, ApprovalScope, Delivery,
    ItemKind, PermissionApply, PermissionMode, RuntimeCapability,
};
use serde_json::{Value, json};
use std::path::PathBuf;

/// Where the slash-command catalogue lives on disk. See the note in `capability()`.
const COMMAND_CACHE: &str = "claude-commands.json";

/// Is this command **project-scoped** — does its description end with `(project)` or
/// `(project:...)`?
///
/// That is the stamp Claude puts in the catalogue: a user-level command's description ends
/// with `(user)`, and a built-in carries no suffix. Only the trailing parenthesized segment
/// counts, so a description whose body happens to mention "project" is not caught by mistake.
fn is_project_scoped(description: &str) -> bool {
    let d = description.trim_end();
    let Some(open) = d.rfind('(') else {
        return false;
    };
    d.ends_with(')') && d[open..].starts_with("(project")
}

/// Is this command supplied by a **plugin** — is its name `<plugin>:<command>` and its
/// description prefixed with `(<plugin>)`?
///
/// A plugin command carries no trailing `(project)` stamp: in the handshake catalogue they are
/// named `figma:figma-use` and described as `"(figma) **MANDATORY prerequisite** — ..."` — the
/// marker sits at the **start**. [`is_project_scoped`], which reads only the end, stops none of
/// them.
///
/// Both signals must hold together. The leading parenthesized segment alone catches built-ins:
/// the same catalogue describes `/agents` as `"(removed) ..."`. A colon in the name alone
/// catches the user-level commands under `~/.claude/commands/<subdirectory>/` — they are named
/// `<directory>:<command>` too, and they work in every workspace.
fn is_plugin_scoped(name: &str, description: &str) -> bool {
    let Some((plugin, _)) = name.split_once(':') else {
        return false;
    };
    !plugin.is_empty()
        && description
            .trim_start()
            .strip_prefix('(')
            .and_then(|rest| rest.strip_prefix(plugin))
            .is_some_and(|rest| rest.starts_with(')'))
}

/// The machine-level catalogue may only hold commands every workspace can actually use.
///
/// Two kinds stay out, for the same reason: the project commands under `.claude/commands`, and
/// the commands a plugin brings — `claude plugin install -s project` writes the enablement into
/// that repository's own `.claude/settings.json` (`--scope` is user / **project** / local), so
/// those commands exist only inside that repository. No field in the catalogue says which scope
/// an entry has, so a plugin command never enters the machine cache: if one does, that
/// repository's command names and descriptions ride along with `rc.register` to the viewers of
/// every workspace on this machine, where they cannot be used at all.
fn machine_commands(
    commands: Vec<crate::protocol::SlashCommand>,
) -> Vec<crate::protocol::SlashCommand> {
    commands
        .into_iter()
        .filter(|command| {
            let description = command.description.as_deref().unwrap_or_default();
            !is_project_scoped(description) && !is_plugin_scoped(&command.name, description)
        })
        .collect()
}

fn cached_machine_commands() -> Vec<crate::protocol::SlashCommand> {
    let cached = crate::rc::load_json::<Vec<crate::protocol::SlashCommand>>(COMMAND_CACHE);
    let filtered = machine_commands(cached.clone());
    // The on-disk cache can hold project commands, so the return value is filtered
    // unconditionally; the disk copy is then migrated best-effort so every register does not
    // read the same polluted data again. A failed write costs cache hygiene only — completion
    // data must never keep the daemon from starting.
    if filtered != cached {
        let _ = crate::rc::save_json(COMMAND_CACHE, &filtered);
    }
    filtered
}

pub fn capability() -> RuntimeCapability {
    RuntimeCapability {
        runtime: "claude-code".into(),
        available: crate::adapter::which("claude").is_some(),
        version: super::probe_version("claude", &["--version"]),
        steer: Some(Delivery::AtToolBoundary),
        interrupt: true,
        approvals: true,
        partial_messages: true,
        resume: true,
        // The catalogue is **not** asked for here; it is what an earlier session asked for
        // and stored.
        //
        // Capabilities are reported at the moment of `rc.register` — no session has started
        // yet, and the catalogue comes from the harness's `initialize` reply in each session.
        // Asking here leaves this field empty forever, and slash completion in the web
        // interface **never has any data** (`cap?.commands` is always `[]`).
        //
        // Save a copy to disk and report it next time: the first connection after an install
        // is still empty, and once a session has run it stays, across restarts too. The
        // degradation is honest (no data, no completion) rather than passing an empty array
        // off as "this runtime has no slash commands".
        commands: cached_machine_commands(),
        // All five map onto native `--permission-mode` values, and the switch
        // is live: a `set_permission_mode` control request is answered
        // `{"mode":"auto"}` and the very next `system/init` reports the new
        // mode.
        permission_modes: vec![
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
            PermissionMode::Plan,
            PermissionMode::Bypass,
        ],
        permission_switch: Some(PermissionApply::Immediate),
    }
}

/// [`PermissionMode`] → the native `--permission-mode` value.
///
/// `default` is not in `claude --help`'s choice list (which shows `acceptEdits`,
/// `auto`, `bypassPermissions`, `manual`, `dontAsk`, `plan`) but is still
/// accepted: the process starts and `system/init` reports
/// `permissionMode: default`. Keeping it is deliberate: it is the only value
/// that means "whatever this machine's own configuration says".
fn native_mode(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Auto => "auto",
        PermissionMode::Plan => "plan",
        PermissionMode::Bypass => "bypassPermissions",
    }
}

/// The native → neutral direction, for reading a `setMode` suggestion back.
fn neutral_mode(native: &str) -> Option<PermissionMode> {
    Some(match native {
        "default" => PermissionMode::Default,
        "acceptEdits" => PermissionMode::AcceptEdits,
        "auto" => PermissionMode::Auto,
        "plan" => PermissionMode::Plan,
        "bypassPermissions" => PermissionMode::Bypass,
        // `manual` / `dontAsk` exist natively but have no neutral spelling; a
        // suggestion for one is simply not offered as "stop asking".
        _ => return None,
    })
}

/// Which mode a `permission_suggestions` array would put the session in.
///
/// `None` means the request came without a suggestion we can act on, and the
/// "allow, and stop asking" button must not be offered for it.
fn suggested_mode(suggestions: &[Value]) -> Option<PermissionMode> {
    // A suggestion array we can act on has exactly one setMode entry. Multiple
    // native effects have ordering semantics we have not measured, so guessing even
    // conservatively would make `suggested_permission_mode` cease to be the
    // exact effect the daemon is authorizing.
    if suggestions.len() != 1 {
        return None;
    }
    let suggestion = &suggestions[0];
    // Echoing an unmodelled rule under `updatedPermissions` could loosen more
    // than the daemon persisted. Do not offer session scope unless the native
    // effect is a mode we can account for first.
    if suggestion.get("type").and_then(|t| t.as_str()) != Some("setMode") {
        return None;
    }
    // `destination` decides how long the effect lives, so it is part of "an
    // effect we can account for" exactly like `type` and `mode` are. The only
    // value we account for is `"session"`; `"userSettings"` (and any other
    // spelling, including a missing one) writes a mode that outlives this RC session and
    // applies to every later `claude` run on this machine. Everything
    // downstream — the card's `can_allow_for_session`, `ApprovalOutcome::
    // Applied { effective_mode }`, the supervisor's `effective_mode ==
    // expected_mode` check — accounts for it as session-scoped and would
    // compare equal, so nothing would ever record that the real effect was
    // wider. Not offering the button costs one extra prompt; offering it costs
    // a machine-wide permission write nobody agreed to.
    if suggestion.get("destination").and_then(|d| d.as_str()) != Some("session") {
        return None;
    }
    suggestion
        .get("mode")
        .and_then(|m| m.as_str())
        .and_then(neutral_mode)
}

/// Classify only the matching control response for a mode change.
///
/// Only the protocol's explicit `error` response proves that native policy did
/// not change.
/// A success naming a different mode is itself evidence of some side effect;
/// malformed or mismatched success therefore stays outcome-unknown.
fn permission_mode_response(
    v: &Value,
    request_id: &str,
    requested: PermissionMode,
) -> Option<Result<(), PermissionModeChangeError>> {
    if v.get("type").and_then(|x| x.as_str()) != Some("control_response") {
        return None;
    }
    let resp = v.get("response")?;
    if resp.get("request_id").and_then(|x| x.as_str()) != Some(request_id) {
        return None;
    }
    match resp.get("subtype").and_then(|x| x.as_str()) {
        Some("success") => {
            let echoed = resp
                .get("response")
                .and_then(|r| r.get("mode"))
                .and_then(|m| m.as_str());
            match echoed {
                Some(got) if got == native_mode(requested) => Some(Ok(())),
                Some(got) => Some(Err(PermissionModeChangeError::outcome_unknown(format!(
                    "claude reported switching to `{got}`, not `{}`",
                    native_mode(requested)
                )))),
                None => Some(Err(PermissionModeChangeError::outcome_unknown(
                    "claude acknowledged the permission-mode change without reporting the resulting mode",
                ))),
            }
        }
        Some("error") => Some(Err(PermissionModeChangeError::refused(format!(
            "claude refused the permission-mode change: {}",
            resp.get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("no reason given")
        )))),
        other => Some(Err(PermissionModeChangeError::outcome_unknown(format!(
            "claude returned an unrecognized permission-mode response subtype: {}",
            other.unwrap_or("<missing>")
        )))),
    }
}

/// One outstanding `can_use_tool` request.
struct PendingApproval {
    request_id: String,
    /// Verbatim `permission_suggestions` from the request, if it carried any.
    suggestions: Vec<Value>,
}

pub struct ClaudeCodeDriver {
    proc: Proc,
    session_id: String,
    cwd: PathBuf,
    /// Turn id we synthesize. claude-code has no turn id of its own on the
    /// stream, so we mint one per user message and close it on `result`.
    current_turn: Option<String>,
    /// `tool_use_id` → the control `request_id` we must answer with, plus the
    /// `permission_suggestions` that came with it.
    ///
    /// The suggestions have to be kept because "allow, and stop asking" is
    /// expressed by **echoing them back** in the response, and the shape is
    /// `[{"type":"setMode","mode":"acceptEdits","destination":"session"}]`.
    /// Inventing that object ourselves would be guessing at a private format;
    /// echoing what the CLI just offered cannot drift.
    pending_approvals: std::collections::HashMap<String, PendingApproval>,
    /// Current permission mode, kept so viewers joining late render the right
    /// control without asking the harness.
    mode: PermissionMode,
    /// Set once the first `system/init` arrives.
    ready_sent: bool,
    /// Slash commands the CLI advertised in its handshake. Surfaced to viewers
    /// so the web composer can autocomplete `/goal`, `/compact`, … exactly like
    /// the terminal does — this is what makes the web feel like the terminal.
    pub commands: Vec<crate::protocol::SlashCommand>,
    /// Lines read while waiting for a control response, not yet handed upward.
    ///
    /// `set_permission_mode` waits for the CLI to confirm, and other lines keep arriving on
    /// the stream before that confirmation. Dropping them leaves a hole in the transcript, so
    /// they are held here and `next_event` releases them first — order is unchanged.
    pushback: Pushback,
    /// Messages we sent that `--replay-user-messages` will echo back, in send
    /// order.
    ///
    /// Without this the echo looks like a fresh local turn and the UI renders
    /// two turn headers for one turn. We still *want* the echo — it is how a
    /// second viewer learns what the driver typed — we just must not count it
    /// as a new turn.
    ///
    /// A queue, not a single slot: two steers sent back to back mid-turn would push the first
    /// out of a single slot, and that first echo would then count as a fresh local user
    /// message — a second turn header drawn out of nowhere inside one turn, and the transcript
    /// attributed to the wrong place. Echoes come back in the order they were written to
    /// stdin, so only the head of the queue is compared against.
    awaiting_echoes: std::collections::VecDeque<String>,
}

/// The exact command line `launch` runs, minus the spawn.
///
/// Split out so a test can assert against **the argv that actually runs** — a copy
/// transcribed into the test only ever pins the copy.
fn argv(spec: &LaunchSpec, session_id: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-p".into(),
        "--input-format".into(),
        "stream-json".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--include-partial-messages".into(),
        "--replay-user-messages".into(),
        "--permission-prompt-tool".into(),
        "stdio".into(),
    ];
    let mode = spec.effective_mode();
    if mode.is_dangerous() {
        args.push("--dangerously-skip-permissions".into());
    } else {
        args.push("--permission-mode".into());
        args.push(native_mode(mode).into());
    }
    if let Some(m) = &spec.model {
        args.push("--model".into());
        args.push(m.clone());
    }
    match &spec.resume_from {
        Some(id) => {
            args.push("--resume".into());
            args.push(id.clone());
        }
        None => {
            args.push("--session-id".into());
            args.push(session_id.to_string());
        }
    }
    args
}

impl ClaudeCodeDriver {
    /// Start a claude-code session. Failure comes back as [`LaunchError`], propagating
    /// "did the OS spawn happen" unchanged: an error from `Proc::spawn` is **provably nothing
    /// started**, while a failed handshake write leaves a process already running.
    pub async fn launch(spec: LaunchSpec) -> Result<ClaudeCodeDriver, LaunchError> {
        let session_id = spec
            .resume_from
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

        let args = argv(&spec, &session_id);
        let mode = spec.effective_mode();

        let mut env = super::lineage_env(spec.agit_session.as_ref());
        // Mark the child so nested tooling can tell it is running under RC.
        env.push(("AGIT_RC".into(), "1".into()));

        let mut proc = Proc::spawn("claude", &args, &spec.cwd, &env)?;

        // The SDK handshake. Declaring it is what puts the CLI in "ask the
        // client" mode for permissions; without it we would silently fall back
        // to whatever the local settings.json says, which is exactly the
        // failure mode where a shared workspace quietly self-approves.
        proc.write_line(&json!({
            "type": "control_request",
            "request_id": "agit-init",
            "request": { "subtype": "initialize", "hooks": {} }
        }))
        .await
        .map_err(LaunchError::spawned)?;

        Ok(ClaudeCodeDriver {
            proc,
            session_id,
            cwd: spec.cwd,
            current_turn: None,
            pending_approvals: Default::default(),
            mode,
            ready_sent: false,
            commands: vec![],
            pushback: Default::default(),
            awaiting_echoes: Default::default(),
        })
    }

    pub fn runtime_thread_id(&self) -> Option<&str> {
        Some(&self.session_id)
    }

    /// `~/.claude/projects/<cwd-slug>/<session-id>.jsonl`. Known up front
    /// because we chose the session id — no waiting, no globbing.
    pub fn transcript_path(&self) -> Option<PathBuf> {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        Some(
            home.join(".claude/projects")
                .join(crate::adapter::claude_code::slug_for(&self.cwd))
                .join(format!("{}.jsonl", self.session_id)),
        )
    }

    pub async fn start_turn(&mut self, message: &str) -> super::TurnStartOutcome {
        // A running turn is refused, not silently overwritten into `current_turn`. Overwriting
        // orphans the running turn's id: its `result` closes the **new** id and the old turn
        // never gets its own `turn.completed` — that turn spins forever in the web interface,
        // and nothing on the machine settles what it produced. A user message written mid-turn
        // does not open a new turn anyway (it counts into the same turn, see the module docs),
        // so promising a new turn id here is a false report.
        if self.current_turn.is_some() {
            return super::TurnStartOutcome::RetryableNotAccepted {
                message: "a claude turn is already running; steer it or wait for completion".into(),
            };
        }
        // A `turn.start` is refused too while a message still awaits its echo, even with no
        // turn running.
        //
        // That message (a mid-turn steer that lost the race to the end of the turn) is already
        // written to the CLI's stdin, but is consumed as **the next turn's prompt** only after
        // the previous turn's `result`. Accepting a turn now binds the freshly minted id to
        // the native turn **that older message** opens: its echo is eaten by the "swallow it
        // while a turn is open" rule below, so that turn no longer opens itself and its deltas
        // and `result` are all recorded under the new id — the web interface shows a card
        // titled with the new prompt whose body answers the previous steer, `turn.completed`
        // fires a turn early, and the settlement boundary lands on a turn that is not this
        // RPC's. When the real new prompt is consumed, all it can do is open yet another turn
        // id that no RPC claims.
        //
        // The refusal is retryable, not a wedged session: the queued message opens a turn of
        // its own, runs, and sends `turn.completed`; the queue drains and the next
        // `turn.start` goes through cleanly.
        if !self.awaiting_echoes.is_empty() {
            return super::TurnStartOutcome::RetryableNotAccepted {
                message: "a queued claude message has not been echoed back yet; it opens the next \
                          turn — retry once that turn completes"
                    .into(),
            };
        }
        let turn = uuid::Uuid::new_v4().to_string();
        self.current_turn = Some(turn.clone());
        self.awaiting_echoes.push_back(message.to_string());
        match self
            .proc
            .write_line(&json!({"type":"user","message":{"role":"user","content":message}}))
            .await
        {
            Ok(()) => super::TurnStartOutcome::Accepted {
                turn_id: turn,
                still_running: true,
                consumed_mode: None,
                confirmation: super::TurnStartConfirmation::Exact,
            },
            Err(error) => super::TurnStartOutcome::Unknown {
                message: format!("claude turn start outcome is unknown: {error}"),
                attempted_mode: None,
            },
        }
    }

    pub async fn steer(&mut self, message: &str) -> crate::Result<Delivery> {
        // With no turn running this is refused rather than written anyway. Writing anyway
        // makes the CLI open an **invisible** native turn: `current_turn` is None, so its
        // stream events hang off `t:<idx>` and the `result` branch swallows it outright — no
        // turn.started and no turn.completed, viewers never see it run, and settlement does
        // not know to wait for it. The method of the same name on the codex side refuses the
        // same way; the two must mean the same thing.
        if self.current_turn.is_none() {
            anyhow::bail!("no turn is running — send a message instead of steering");
        }
        self.awaiting_echoes.push_back(message.to_string());
        if let Err(error) = self
            .proc
            .write_line(&json!({"type":"user","message":{"role":"user","content":message}}))
            .await
        {
            // A write that did not succeed withdraws its echo. The queue compares against
            // the **head** only, so an echo that will never arrive sits at the head forever
            // and misaligns every later steer's echo: each one is redrawn as a fresh local
            // user message inside the running turn, the queue grows by one every time, and
            // this session never heals again. (A single slot heals when the next write
            // overwrites it; a queue has no such way out.) For the ambiguous failure where
            // the bytes did land, the cost is that its echo is drawn once as a local user
            // message — once, and it ends there; far cheaper than misattributing the whole
            // session for good.
            self.awaiting_echoes.pop_back();
            return Err(error);
        }
        // Measured, not assumed — see the module docs.
        Ok(Delivery::AtToolBoundary)
    }

    pub async fn interrupt(&mut self) -> crate::Result<()> {
        self.proc
            .write_line(&json!({
                "type":"control_request",
                "request_id": uuid::Uuid::new_v4().to_string(),
                "request": {"subtype":"interrupt"}
            }))
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
        let mut mode_after_write = None;
        let body = match r.decision {
            ApprovalDecision::Allow => {
                let mut body = json!({"behavior":"allow"});
                // "Allow, and stop asking" = echo the suggestions the CLI
                // attached to this very request: answering with
                // `updatedPermissions: [{"type":"setMode","mode":"acceptEdits",
                // "destination":"session"}]` is accepted and the next edit
                // lands without a prompt.
                if r.scope == ApprovalScope::Session {
                    let Some(mode) = suggested_mode(&pending.suggestions) else {
                        return ApprovalOutcome::ExplicitRefusal {
                            message: "this approval has no fully understood session permission-mode suggestion"
                                .into(),
                            retained: false,
                        };
                    };
                    body["updatedPermissions"] = Value::Array(pending.suggestions.clone());
                    mode_after_write = Some(mode);
                }
                body
            }
            ApprovalDecision::Deny => json!({
                "behavior":"deny",
                "message": r.message.clone().unwrap_or_else(|| "denied by a reviewer in the shared workspace".into())
            }),
        };
        let write = self
            .proc
            .write_line(&json!({
                "type":"control_response",
                "response":{"subtype":"success","request_id":pending.request_id,"response":body}
            }))
            .await;
        // Do not move our authoritative projection before even the write has
        // succeeded. A partial/uncertain write keeps the daemon's danger arm,
        // but it must not make later UI events claim a confirmed mode.
        match write {
            Ok(()) => {
                if let Some(mode) = mode_after_write {
                    self.mode = mode;
                }
                ApprovalOutcome::Applied {
                    effective_mode: mode_after_write,
                }
            }
            Err(error) => ApprovalOutcome::Unknown {
                message: format!("claude approval outcome is unknown: {error}"),
                attempted_mode: mode_after_write,
            },
        }
    }

    /// Switch permission mode mid-session.
    ///
    /// The request is answered
    /// `{"subtype":"success","response":{"mode":"auto"}}` and takes effect at
    /// once — a `Write` that prompted under `default` goes straight through
    /// under `auto`.
    pub async fn set_permission_mode(
        &mut self,
        mode: PermissionMode,
    ) -> PermissionModeChangeResult {
        let request_id = uuid::Uuid::new_v4().to_string();
        self.proc
            .write_line(&json!({
                "type": "control_request",
                "request_id": request_id,
                "request": {"subtype": "set_permission_mode", "mode": native_mode(mode)}
            }))
            .await
            .map_err(|e| PermissionModeChangeError::outcome_unknown(e.to_string()))?;

        // **Report success only once the CLI confirms.**
        //
        // Writing without waiting leaves the web interface and the roster showing "applied"
        // when the CLI refuses, errors, or exits right after the write. Tightening back from
        // `bypass` is where that is fatal: a process that still runs with no checks is dressed
        // up by the interface as a protected one. The failure must travel back along the RPC.
        //
        // Other lines (deltas, items, ...) keep arriving on the stream before the matching
        // response and must not be lost — they are held here, and `next_event` releases them
        // first, order unchanged.
        let deadline = std::time::Duration::from_secs(10);
        let started = std::time::Instant::now();
        loop {
            let left = deadline.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Err(PermissionModeChangeError::outcome_unknown(format!(
                    "claude did not confirm the permission-mode change within {}s",
                    deadline.as_secs()
                )));
            }
            let Ok(Some(queued)) = tokio::time::timeout(left, self.proc.next()).await else {
                return Err(PermissionModeChangeError::outcome_unknown(
                    "claude did not confirm the permission-mode change",
                ));
            };
            if let Line::Json(v) = queued.line()
                && let Some(outcome) = permission_mode_response(v, &request_id, mode)
            {
                outcome?;
                self.mode = mode;
                return Ok(PermissionApply::Immediate);
            }
            // Not the line we are waiting for: keep it (anything past the pushback cap is
            // dropped and counted, otherwise the bytes piling up block the reader forever and
            // the confirmation we are waiting for never arrives).
            self.pushback.push(queued);
        }
    }

    pub fn permission_mode(&self) -> PermissionMode {
        self.mode
    }

    pub async fn next_event(&mut self) -> Option<HarnessEvent> {
        loop {
            // Release what was held while waiting for a control response before reading
            // anything new — the order is the order on the stream.
            let line = match self.pushback.pop_front() {
                Some(line) => line,
                None => self.proc.next().await?.into_line(),
            };
            let v = match line {
                Line::Eof => {
                    return Some(HarnessEvent::Exited {
                        code: self.proc.wait().await,
                    });
                }
                Line::Notice(t) => return Some(HarnessEvent::Notice { text: t }),
                // A critical frame (`result`, an approval `control_request`, ...) that could
                // not be held while waiting for a control response. Reporting it as an
                // ordinary notice leaves the supervisor waiting forever for a turn end that
                // never comes; the whole generation must fail-stop here.
                Line::Fatal(message) => {
                    return Some(HarnessEvent::ProtocolInvariant {
                        message,
                        attempted_mode: None,
                        confirmation_token: None,
                    });
                }
                Line::Json(v) => v,
            };
            if let Some(ev) = self.classify(v) {
                return Some(ev);
            }
        }
    }

    /// Map one stream-json frame to a harness event. `None` means "internal,
    /// keep reading" — heartbeats, token counters, and the like.
    fn classify(&mut self, v: Value) -> Option<HarnessEvent> {
        let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        match ty {
            "system" => {
                let sub = v.get("subtype").and_then(|x| x.as_str()).unwrap_or("");
                if sub == "init" && !self.ready_sent {
                    self.ready_sent = true;
                    if let Some(sid) = v.get("session_id").and_then(|x| x.as_str()) {
                        self.session_id = sid.to_string();
                    }
                    return Some(HarnessEvent::Ready {
                        runtime_thread_id: self.session_id.clone(),
                        transcript_path: self.transcript_path(),
                    });
                }
                None
            }

            // The handshake reply carries the slash-command catalogue.
            "control_response" => {
                let r = v.get("response")?.get("response")?;
                let arr = r.get("commands")?.as_array()?;
                let harvested = machine_commands(
                    arr.iter()
                        .filter_map(|c| {
                            let description = c
                                .get("description")
                                .and_then(|x| x.as_str())
                                .map(String::from);
                            Some(crate::protocol::SlashCommand {
                                name: c.get("name")?.as_str()?.to_string(),
                                description,
                                argument_hint: c
                                    .get("argumentHint")
                                    .and_then(|x| x.as_str())
                                    .filter(|s| !s.is_empty())
                                    .map(String::from),
                            })
                        })
                        .collect(),
                );
                // Save a copy so **the next** `rc.register` can report it.
                //
                // Capabilities are reported at the moment of registration, while the catalogue
                // is known only once this session has finished its handshake — the two are one
                // session apart by construction. Writing it down is the cheapest way to close
                // that gap: the first connection after an install has no completion, and once
                // a session has run it stays, across restarts too.
                //
                // A failed write does not matter: the next handshake asks again, and the cost
                // is completion appearing one session later.
                if !harvested.is_empty() && harvested != self.commands {
                    let _ = crate::rc::save_json(COMMAND_CACHE, &harvested);
                }
                self.commands = harvested;
                None
            }

            "control_request" => {
                let req = v.get("request")?;
                if req.get("subtype").and_then(|x| x.as_str()) != Some("can_use_tool") {
                    return None;
                }
                let request_id = v.get("request_id").and_then(|x| x.as_str())?.to_string();
                let tool = req
                    .get("tool_name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("tool")
                    .to_string();
                let input = req.get("input").cloned().unwrap_or(Value::Null);
                let approval_id = req
                    .get("tool_use_id")
                    .and_then(|x| x.as_str())
                    .map(String::from)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                let suggestions: Vec<Value> = req
                    .get("permission_suggestions")
                    .and_then(|x| x.as_array())
                    .cloned()
                    .unwrap_or_default();
                // Only a `setMode` suggestion means "and stop asking". Other
                // suggestion kinds may appear; offering the button for one we
                // cannot honour would be a lie on the card.
                let suggested_permission_mode = suggested_mode(&suggestions);
                let can_allow_for_session = suggested_permission_mode.is_some();
                self.pending_approvals.insert(
                    approval_id.clone(),
                    PendingApproval {
                        request_id,
                        suggestions,
                    },
                );

                let paths = super::paths_of(&input);
                // Three classes, because they route differently (PRD §5.2):
                // Exec and FileChange may be delegated to operators, but reaching
                // the network or outside the allowlist always goes to the owner.
                let kind = if !paths.is_empty() {
                    ApprovalKind::FileChange
                } else if matches!(tool.as_str(), "WebFetch" | "WebSearch") {
                    ApprovalKind::PermissionEscalation
                } else {
                    ApprovalKind::Exec
                };
                Some(HarnessEvent::Approval(ApprovalRequest {
                    approval_id,
                    session_id: String::new(), // filled in by the supervisor
                    turn_id: self.current_turn.clone().unwrap_or_default(),
                    kind,
                    summary: super::summarize_tool(&tool, &input),
                    tool,
                    input,
                    paths,
                    timeout_secs: 0, // claude-code blocks indefinitely; the hub imposes the clock
                    // The driver cannot decide this — it does not know the workspace
                    // allowlist. The supervisor overwrites it once it has the event.
                    // Defaulting to true is fail-closed: a path that misses the overwrite
                    // asks the owner one time too many, not one time too few.
                    requires_owner: true,
                    // The supervisor overwrites these two fields with the real test before
                    // redaction. The initial value here is the fail-closed side.
                    owner_reason: Some(crate::protocol::OwnerReason::Unprovable),
                    can_allow_for_session,
                    suggested_permission_mode,
                    requested_at: chrono::Utc::now().to_rfc3339(),
                }))
            }

            // Token-level streaming.
            "stream_event" => {
                let ev = v.get("event")?;
                let et = ev.get("type").and_then(|x| x.as_str()).unwrap_or("");
                match et {
                    "content_block_start" => {
                        let idx = ev.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
                        let cb = ev.get("content_block")?;
                        let (kind, tool) =
                            match cb.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                                "text" => (ItemKind::AssistantMessage, None),
                                "thinking" | "redacted_thinking" => (ItemKind::Reasoning, None),
                                "tool_use" => (
                                    ItemKind::ToolCall,
                                    cb.get("name").and_then(|x| x.as_str()).map(String::from),
                                ),
                                _ => (ItemKind::Other, None),
                            };
                        Some(HarnessEvent::ItemStarted {
                            item_id: self.item_id(idx),
                            kind,
                            tool,
                        })
                    }
                    "content_block_delta" => {
                        let idx = ev.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
                        let d = ev.get("delta")?;
                        // Signature deltas are cryptographic padding — streaming
                        // them to a browser is pure noise.
                        let text = d
                            .get("text")
                            .or_else(|| d.get("thinking"))
                            .or_else(|| d.get("partial_json"))
                            .and_then(|x| x.as_str())?;
                        Some(HarnessEvent::Delta {
                            item_id: self.item_id(idx),
                            text: text.to_string(),
                        })
                    }
                    "content_block_stop" => {
                        let idx = ev.get("index").and_then(|x| x.as_u64()).unwrap_or(0);
                        Some(HarnessEvent::ItemCompleted {
                            item_id: self.item_id(idx),
                        })
                    }
                    _ => None,
                }
            }

            // A user line coming back at us. With --replay-user-messages this is
            // how every viewer learns what the driver typed.
            "user" => {
                let content = v.get("message").and_then(|m| m.get("content"))?;
                let text = match content {
                    Value::String(s) => Some(s.clone()),
                    Value::Array(a) => a
                        .iter()
                        .find(|b| b.get("type").and_then(|x| x.as_str()) == Some("text"))
                        .and_then(|b| b.get("text").and_then(|x| x.as_str()).map(String::from)),
                    _ => None,
                }?;
                if text.starts_with("[Request interrupted") {
                    // No open turn = this interrupt closed nothing (the turn ended on its
                    // own and the interrupt's echo arrived a step late). Sending
                    // `turn.completed` here is **forbidden**: see the ghost-turn argument in
                    // the `result` branch below.
                    let turn = self.current_turn.take()?;
                    return Some(HarnessEvent::TurnCompleted {
                        turn_id: turn,
                        outcome: TurnOutcome::Interrupted,
                        cost_usd: None,
                        duration_ms: None,
                    });
                }
                // Our own message coming back through --replay-user-messages.
                // Echoes come back in write order, so only the head is consumed; a match
                // deeper in the queue means the order broke, and it is better to surface it
                // as a new message.
                let echoed = self.awaiting_echoes.front() == Some(&text);
                if echoed {
                    self.awaiting_echoes.pop_front();
                }
                // **Swallow an echo only while the turn is still open**: that turn's
                // turn.started went out already, and a second one draws two turn headers for
                // one turn.
                //
                // An echo of our own arriving after the turn closed means this mid-turn steer
                // sat in stdin the whole time and the CLI consumed it as **the next turn's
                // prompt** only after the previous `result`. Swallowing it then makes that
                // whole turn invisible to everyone: `current_turn` is None, stream events hang
                // off `t:<idx>`, and its `result` is swallowed by the `take()?` below — the
                // web interface shows idle and settlement does not know another turn is still
                // coming. So fall through and open the turn as usual: this echo is that turn's
                // prompt.
                if echoed && self.current_turn.is_some() {
                    return None;
                }
                let turn = self
                    .current_turn
                    .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
                    .clone();
                Some(HarnessEvent::TurnStarted {
                    turn_id: turn,
                    prompt: Some(text),
                })
            }

            "result" => {
                // **With no open turn, nothing is closed.**
                //
                // After an interrupt the CLI still sends a `result` for that turn, and a
                // command dropped into the background (a `sleep` does it) makes it send a
                // second one. With `unwrap_or_default()` as the backstop here, every extra
                // `result` draws a `turn.completed` carrying `turn_id: ""` — a ghost turn with
                // no matching `turn.started`. In the web interface that reads as "one
                // interrupt draws two end lines, and the second one says the turn ended fine";
                // on the machine it is the same commit settled twice.
                //
                // The test is `current_turn` rather than a "just interrupted" flag: a flag
                // stops only **the one** that comes straight after, and the extra `result`s
                // are more than one.
                let turn = self.current_turn.take()?;
                let is_err = v.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
                Some(HarnessEvent::TurnCompleted {
                    turn_id: turn,
                    outcome: if is_err {
                        TurnOutcome::Error
                    } else {
                        TurnOutcome::Ok
                    },
                    cost_usd: v.get("total_cost_usd").and_then(|x| x.as_f64()),
                    duration_ms: v.get("duration_api_ms").and_then(|x| x.as_u64()),
                })
            }

            _ => None,
        }
    }

    /// Block index → a stable item id within the current turn.
    fn item_id(&self, index: u64) -> String {
        format!("{}:{index}", self.current_turn.as_deref().unwrap_or("t"))
    }

    pub async fn shutdown(&mut self) -> crate::Result<()> {
        self.proc.shutdown().await
    }

    #[cfg(test)]
    pub(crate) fn test_driver() -> ClaudeCodeDriver {
        ClaudeCodeDriver {
            proc: Proc::spawn("cat", &[], &PathBuf::from("/"), &[]).expect("test process"),
            session_id: "test-session".into(),
            cwd: PathBuf::from("/"),
            current_turn: Some("test-turn".into()),
            pending_approvals: Default::default(),
            mode: PermissionMode::Default,
            ready_sent: true,
            commands: vec![],
            pushback: Default::default(),
            awaiting_echoes: Default::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn clear_test_current_turn(&mut self) {
        self.current_turn = None;
    }

    #[cfg(test)]
    pub(crate) fn add_test_approval(
        &mut self,
        approval_id: &str,
        suggested_mode: Option<PermissionMode>,
    ) {
        let suggestions = suggested_mode
            .map(|mode| {
                vec![json!({
                    "type": "setMode",
                    "mode": native_mode(mode),
                    "destination": "session"
                })]
            })
            .unwrap_or_default();
        self.pending_approvals.insert(
            approval_id.into(),
            PendingApproval {
                request_id: "native-request".into(),
                suggestions,
            },
        );
    }

    #[cfg(test)]
    pub(crate) fn fail_test_write_outcomes(&mut self, count: usize) {
        self.proc.fail_write_outcomes(count);
    }

    #[cfg(test)]
    pub(crate) fn fail_test_shutdowns(
        &mut self,
        count: usize,
    ) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        self.proc.fail_shutdowns(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A project-scoped command never enters the machine-level catalogue.**
    ///
    /// The catalogue rides along with `rc.register` to every workspace on this machine, while
    /// a command under `.claude/commands` belongs to the current project alone. Taking every
    /// entry as offered lets the last project to complete a handshake write its own commands
    /// into the machine-wide cache — one project's command names and descriptions leak to the
    /// viewers of another workspace.
    ///
    /// The test is the stamp Claude puts at the end of the description. Only the **trailing**
    /// parenthesized segment counts: a description whose body happens to mention "project"
    /// (the last assertion) must not be caught by mistake.
    #[test]
    fn a_project_scoped_command_never_enters_the_machine_catalogue() {
        assert!(is_project_scoped("Deploy the staging site. (project)"));
        assert!(is_project_scoped(
            "Frontend release steps. (project:frontend)"
        ));
        assert!(!is_project_scoped(
            "Version control for agent sessions. (user)"
        ));
        assert!(!is_project_scoped("Compact the conversation"));
        assert!(!is_project_scoped("Manage the project board. (user)"));
    }

    /// **A plugin command does not enter the machine-level catalogue either.**
    ///
    /// A plugin can be enabled per repository (`claude plugin install -s project` writes that
    /// repository's own `.claude/settings.json`), and no field in the handshake catalogue says
    /// which scope an entry has. Letting one in sends that repository's plugin command names
    /// and descriptions along with `rc.register` to the viewers of every workspace on this
    /// machine — exactly the cross-workspace leak this filter claims to prevent, and they
    /// cannot use them there anyway.
    ///
    /// The test reads two signals together. In the handshake catalogue a plugin command is
    /// named `figma:figma-use`, its description **starts** with `(figma)` and carries no
    /// trailing `(project)` stamp; a leading parenthesized segment is not by itself a plugin —
    /// the built-in `/agents` description starts with `(removed)` — and a colon in the name is
    /// not by itself a plugin either — the user commands inside a subdirectory carry a colon
    /// too.
    #[test]
    fn a_plugin_scoped_command_never_enters_the_machine_catalogue() {
        let command = |name: &str, description: &str| crate::protocol::SlashCommand {
            name: name.into(),
            description: Some(description.into()),
            argument_hint: None,
        };
        let kept = machine_commands(vec![
            command(
                "figma:figma-use",
                "(figma) **MANDATORY prerequisite** — you MUST invoke this skill …",
            ),
            command(
                "frontend-design:frontend-design",
                "(frontend-design) Guidance …",
            ),
            command("deploy", "Deploy the staging site. (project)"),
            command("agents", "(removed) Ask Claude to create/manage subagents"),
            command("release:notes", "Draft the release notes. (user)"),
            command("compact", "Compact the conversation"),
        ]);
        assert_eq!(
            kept.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["agents", "release:notes", "compact"],
            "a plugin command can be enabled in one repository only, so it must not be \
             advertised to every workspace on this machine"
        );
    }

    /// The first `rc.register` after an upgrade happens before any new session handshake, so
    /// what it reads is whatever the disk cache already holds. Filtering must happen again at
    /// the read entry point; it cannot wait for some future harvest.
    #[test]
    fn a_polluted_legacy_cache_is_filtered_before_registration_and_migrated() {
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || {
            let command = |name: &str, description: &str| crate::protocol::SlashCommand {
                name: name.into(),
                description: Some(description.into()),
                argument_hint: None,
            };
            let polluted = vec![
                command("deploy-secret", "Deploy this repository. (project:app)"),
                command("goal", "Manage a durable goal. (user)"),
                command("help", "Show help"),
            ];
            crate::rc::save_json(COMMAND_CACHE, &polluted).unwrap();

            let advertised = capability().commands;
            assert_eq!(
                advertised
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["goal", "help"],
                "project commands must not reach the first register after upgrade"
            );
            let migrated =
                crate::rc::load_json::<Vec<crate::protocol::SlashCommand>>(COMMAND_CACHE);
            assert_eq!(
                migrated, advertised,
                "the legacy cache should be sanitized once"
            );
        });
    }

    /// The flag set is load-bearing and easy to regress. `--permission-prompt-tool
    /// stdio` in particular: drop it and the CLI stops asking, which turns a
    /// shared workspace into one that silently self-approves.
    #[test]
    fn the_invocation_keeps_the_flags_that_were_measured_to_matter() {
        let spec = LaunchSpec {
            cwd: PathBuf::from("/tmp"),
            resume_from: None,
            agit_session: None,
            model: None,
            dangerous: false,
            permission_mode: None,
        };
        let args = argv(&spec, "sid");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--permission-mode" && w[1] == "default")
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--permission-prompt-tool" && w[1] == "stdio")
        );
        assert!(args.contains(&"--include-partial-messages".to_string()));
        assert!(args.contains(&"--replay-user-messages".to_string()));
        assert!(!args.contains(&"--dangerously-skip-permissions".to_string()));
    }

    /// A driver that exists only to feed `classify()`: `cat` stands in for `Proc`, and these
    /// tests never read its output.
    fn probe() -> ClaudeCodeDriver {
        ClaudeCodeDriver {
            proc: Proc::spawn("cat", &[], &PathBuf::from("/"), &[]).unwrap(),
            session_id: "sid".into(),
            cwd: PathBuf::from("/"),
            current_turn: None,
            pending_approvals: Default::default(),
            mode: PermissionMode::Default,
            ready_sent: true,
            commands: vec![],
            pushback: Default::default(),
            awaiting_echoes: Default::default(),
        }
    }

    fn interrupted_line() -> Value {
        json!({"type":"user","message":{"role":"user",
               "content":[{"type":"text","text":"[Request interrupted by user]"}]}})
    }

    /// One interrupt draws one end line.
    ///
    /// After an interrupt the CLI still sends a `result` for that turn, and a command dropped
    /// into a background task (a long `sleep` does it) makes it send another. No extra
    /// `result` may become a `turn.completed` — in the web interface that is "one interrupt
    /// ended twice", on the machine it is the same commit settled twice.
    #[tokio::test]
    async fn an_interrupt_closes_the_turn_once_no_matter_how_many_results_follow() {
        let mut d = probe();
        d.current_turn = Some("t1".into());

        let ev = d
            .classify(interrupted_line())
            .expect("an interrupt closes the running turn");
        match ev {
            HarnessEvent::TurnCompleted {
                turn_id, outcome, ..
            } => {
                assert_eq!(turn_id, "t1");
                assert!(matches!(outcome, TurnOutcome::Interrupted));
            }
            other => panic!("expected TurnCompleted, got {other:?}"),
        }

        // The interrupt's own echo, and the one a background task sends as it finishes.
        // Neither closes the turn a second time.
        assert!(
            d.classify(json!({"type":"result","is_error":true}))
                .is_none(),
            "the interrupt's echo must not draw a second end line"
        );
        assert!(
            d.classify(json!({"type":"result","is_error":false,"total_cost_usd":0.2}))
                .is_none(),
            "nor may the `result` from a finishing background task draw one"
        );
    }

    /// The other direction: when the interrupt's echo arrives after the `result`, that turn
    /// is already closed and the interrupt must not add a ghost end line carrying
    /// `turn_id: ""`.
    #[tokio::test]
    async fn an_interrupt_that_lands_after_the_turn_already_ended_closes_nothing() {
        let mut d = probe();
        d.current_turn = Some("t1".into());
        assert!(
            d.classify(json!({"type":"result","is_error":false}))
                .is_some()
        );
        assert!(d.classify(interrupted_line()).is_none());
    }

    /// Exercise the real control-request classifier, not a hand-built map:
    /// once the supervisor declares the turn over, the native request id must
    /// disappear as well and a late browser click must be rejected locally.
    #[tokio::test]
    async fn abandoned_approvals_cannot_be_answered_after_a_turn_boundary() {
        let mut d = probe();
        d.current_turn = Some("turn-1".into());
        let event = d
            .classify(json!({
                "type": "control_request",
                "request_id": "native-request-1",
                "request": {
                    "subtype": "can_use_tool",
                    "tool_name": "Bash",
                    "input": {"command": "git status"},
                    "tool_use_id": "approval-1",
                    "permission_suggestions": []
                }
            }))
            .expect("the production classifier should emit an approval");
        assert!(matches!(event, HarnessEvent::Approval(_)));
        assert_eq!(d.pending_approvals.len(), 1);

        assert_eq!(d.abandon_pending_approvals(), 1);
        assert!(d.pending_approvals.is_empty());
        let stale = ApprovalResponse {
            approval_id: "approval-1".into(),
            session_id: "session-1".into(),
            decision: ApprovalDecision::Allow,
            scope: ApprovalScope::Once,
            message: None,
            by: Some("owner".into()),
        };
        assert!(matches!(
            d.answer_approval(&stale).await,
            ApprovalOutcome::ExplicitRefusal {
                message,
                retained: false,
            } if message.contains("no longer pending")
        ));
        d.shutdown().await.expect("stop test process");
    }

    fn session_approval_response() -> ApprovalResponse {
        ApprovalResponse {
            approval_id: "approval-1".into(),
            session_id: "session-1".into(),
            decision: ApprovalDecision::Allow,
            scope: ApprovalScope::Session,
            message: None,
            by: Some("owner".into()),
        }
    }

    fn seed_session_approval(d: &mut ClaudeCodeDriver) {
        d.pending_approvals.insert(
            "approval-1".into(),
            PendingApproval {
                request_id: "native-request-1".into(),
                suggestions: vec![json!({
                    "type": "setMode",
                    "mode": "acceptEdits",
                    "destination": "session"
                })],
            },
        );
    }

    #[tokio::test]
    async fn a_successful_session_approval_reports_its_exact_effective_mode() {
        let mut d = probe();
        seed_session_approval(&mut d);

        assert_eq!(
            d.answer_approval(&session_approval_response()).await,
            ApprovalOutcome::Applied {
                effective_mode: Some(PermissionMode::AcceptEdits),
            }
        );
        assert_eq!(d.permission_mode(), PermissionMode::AcceptEdits);
        d.shutdown().await.expect("stop test process");
    }

    /// **`destination` decides how long this "stop asking" lives, so it is a test exactly as
    /// `type` is.**
    ///
    /// The only scope recognized is `destination: "session"`. The same `setMode` with
    /// `"userSettings"` makes the native side write a mode that is **cross-session and
    /// cross-process**: once this RC session ends, every `claude` run on this machine (inside
    /// RC or not) carries it. The daemon side accounts for it as session-scoped from the card
    /// through to the finish — `can_allow_for_session` lit, the docs calling
    /// `ApprovalOutcome::Applied { effective_mode }` the session's exact mode, the supervisor
    /// comparing only `effective_mode == expected_mode` and finding them equal — and **nothing
    /// anywhere records that the scope was not the session at all**.
    ///
    /// So the test must read `destination` too: an unrecognized scope follows the same rule as
    /// an unrecognized `type` / `mode` — "allow, and stop asking" is never offered.
    #[tokio::test]
    async fn a_non_session_destination_never_offers_session_scope() {
        let mut d = probe();
        d.current_turn = Some("turn-1".into());
        let event = d
            .classify(json!({
                "type": "control_request",
                "request_id": "native-request-1",
                "request": {
                    "subtype": "can_use_tool",
                    "tool_name": "Bash",
                    "input": {"command": "rm -rf ~/.cache/x"},
                    "tool_use_id": "approval-1",
                    "permission_suggestions": [{
                        "type": "setMode",
                        "mode": "bypassPermissions",
                        "destination": "userSettings"
                    }]
                }
            }))
            .expect("the production classifier should emit an approval");
        let HarnessEvent::Approval(card) = event else {
            panic!("expected an approval card");
        };
        assert!(
            !card.can_allow_for_session,
            "a machine-wide, on-disk permission write was offered as session scope"
        );
        assert_eq!(
            card.suggested_permission_mode, None,
            "the daemon would record a session mode for an effect that outlives the session"
        );

        // Even when the card's button is bypassed (an out-of-date web client, a replayed
        // response), this suggestion must not be echoed back as `updatedPermissions` — that
        // step is what actually writes the disk.
        assert!(
            matches!(
                d.answer_approval(&session_approval_response()).await,
                ApprovalOutcome::ExplicitRefusal {
                    message,
                    retained: false,
                } if message.contains("no fully understood session permission-mode suggestion")
            ),
            "the suggestion was echoed back despite its unmodelled destination"
        );
        assert_eq!(
            d.permission_mode(),
            PermissionMode::Default,
            "nothing may move the projected mode for a refused suggestion"
        );
        d.shutdown().await.expect("stop test process");
    }

    #[tokio::test]
    async fn a_flushed_session_approval_with_a_failed_receipt_is_outcome_unknown() {
        let mut d = probe();
        seed_session_approval(&mut d);
        d.proc.fail_write_outcomes(1);

        assert!(matches!(
            d.answer_approval(&session_approval_response()).await,
            ApprovalOutcome::Unknown {
                message,
                attempted_mode: Some(PermissionMode::AcceptEdits),
            } if message.contains("ambiguous harness write outcome")
        ));
        assert_eq!(
            d.permission_mode(),
            PermissionMode::Default,
            "an ambiguous write must not be announced as a confirmed mode"
        );
        let delivered = tokio::time::timeout(std::time::Duration::from_secs(1), d.proc.next())
            .await
            .expect("cat receives the flushed native response")
            .expect("cat echoes one line")
            .into_line();
        assert!(matches!(
            delivered,
            Line::Json(value) if value["type"] == "control_response"
        ));
        d.shutdown().await.expect("stop test process");
    }

    /// `--replay-user-messages` is required (other viewers must see what the
    /// driver typed) but its echo must not open a second turn.
    #[tokio::test]
    async fn the_replayed_echo_of_our_own_message_does_not_open_a_second_turn() {
        let mut d = probe();
        d.current_turn = Some("turn-1".into());
        d.awaiting_echoes.push_back("hello".into());
        let user_line =
            |text: &str| json!({"type":"user","message":{"role":"user","content":text}});

        assert!(
            d.classify(user_line("hello")).is_none(),
            "the production classifier emitted a second turn for its own echo"
        );
        assert_eq!(d.current_turn.as_deref(), Some("turn-1"));
        assert!(d.awaiting_echoes.is_empty());

        // Do not turn the fix into a blanket user-frame filter: after this turn ends, a message
        // entered in the local terminal still opens a real turn through the same classifier.
        assert!(
            d.classify(json!({"type":"result","is_error":false}))
                .is_some()
        );
        let local = d
            .classify(user_line("typed in the terminal"))
            .expect("a genuinely local user message should open a turn");
        assert!(matches!(
            local,
            HarnessEvent::TurnStarted {
                prompt: Some(ref prompt),
                ..
            } if prompt == "typed in the terminal"
        ));
        d.shutdown().await.expect("stop test process");
    }

    /// A mid-turn `turn.start` is refused rather than quietly displacing the id of the running
    /// turn.
    ///
    /// Once displaced, the old turn's `result` closes the new id and the old id never gets its
    /// own `turn.completed` — that turn spins forever in the web interface, and settlement
    /// does not know to wait for it.
    #[tokio::test]
    async fn a_turn_start_while_a_turn_is_running_is_refused_not_overwritten() {
        let mut d = probe();
        d.current_turn = Some("running-turn".into());

        let outcome = d.start_turn("a second prompt").await;
        assert!(
            matches!(
                &outcome,
                super::super::TurnStartOutcome::RetryableNotAccepted { message }
                    if message.contains("already running")
            ),
            "expected RetryableNotAccepted, got {outcome:?}"
        );
        assert_eq!(
            d.current_turn.as_deref(),
            Some("running-turn"),
            "the running turn's id must survive a refused turn.start"
        );
        assert!(
            d.awaiting_echoes.is_empty(),
            "a refused start must not arm an echo that will never come"
        );
        // The refusal happens before the stdin write: not a byte reaches cat.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), d.proc.next())
                .await
                .is_err(),
            "a refused turn.start must not reach native stdin"
        );
        d.shutdown().await.expect("stop test process");
    }

    /// Steering an idle session is an error, not a native turn nobody can see.
    ///
    /// Writing anyway leaves `current_turn` at None: that turn's stream events hang off
    /// `t:<idx>` and its `result` is swallowed, with no turn.started and no turn.completed.
    #[tokio::test]
    async fn steering_an_idle_session_is_refused_instead_of_starting_an_invisible_turn() {
        let mut d = probe();
        assert!(d.current_turn.is_none());

        let error = d
            .steer("nothing to steer")
            .await
            .expect_err("steering with no running turn must be an error");
        assert!(error.to_string().contains("no turn is running"));
        assert!(d.awaiting_echoes.is_empty());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), d.proc.next())
                .await
                .is_err(),
            "a refused steer must not reach native stdin"
        );
        d.shutdown().await.expect("stop test process");
    }

    /// Two steers sent back to back within one turn have both echoes absorbed — the second
    /// must not displace the first from the waiting list, or the first one's echo draws a
    /// second turn header inside the same turn.
    #[tokio::test]
    async fn two_steers_in_flight_absorb_both_echoes_in_order() {
        let mut d = probe();
        d.current_turn = Some("turn-1".into());
        d.steer("first").await.expect("first steer");
        d.steer("second").await.expect("second steer");
        let user_line =
            |text: &str| json!({"type":"user","message":{"role":"user","content":text}});

        assert!(
            d.classify(user_line("first")).is_none(),
            "the first echo must still be recognized after a second steer"
        );
        assert!(
            d.classify(user_line("second")).is_none(),
            "the second echo must be recognized too"
        );
        assert!(d.awaiting_echoes.is_empty());
        assert_eq!(d.current_turn.as_deref(), Some("turn-1"));
        d.shutdown().await.expect("stop test process");
    }

    /// A mid-turn steer that loses the race to the end of the turn: the CLI consumes it as
    /// **the next turn's prompt** only after the `result`, then replays it. That echo must
    /// open a visible turn.
    ///
    /// Swallowing it blindly makes the whole turn invisible: `current_turn` is None, stream
    /// events hang off `t:<idx>`, the `result` is swallowed by `take()?` — the web interface
    /// shows idle and settlement does not know to wait for it to finish. This is the same
    /// failure the idle-steer guard claims to prevent, entering from the end-of-turn side.
    #[tokio::test]
    async fn a_steer_echoed_after_its_turn_ended_still_opens_a_visible_turn() {
        let mut d = probe();
        d.current_turn = Some("turn-1".into());
        d.steer("fix it").await.expect("mid-turn steer is accepted");

        // The turn finishes first; the queued message is not consumed yet.
        assert!(
            d.classify(json!({"type":"result","is_error":false}))
                .is_some(),
            "the running turn must complete first"
        );
        assert!(d.current_turn.is_none());

        let started = d
            .classify(json!({"type":"user","message":{"role":"user","content":"fix it"}}))
            .expect("the replayed steer opened a new native turn and must be visible");
        assert!(
            matches!(
                &started,
                HarnessEvent::TurnStarted { prompt: Some(prompt), .. } if prompt == "fix it"
            ),
            "expected a TurnStarted carrying the steer text, got {started:?}"
        );
        assert!(
            d.current_turn.is_some(),
            "the new native turn must be tracked, or its result is swallowed"
        );
        assert!(d.awaiting_echoes.is_empty());
        d.shutdown().await.expect("stop test process");
    }

    /// With the previous turn finished and a mid-turn steer still sitting unconsumed in the
    /// CLI's stdin, a new `turn.start` is refused rather than handed a freshly minted id.
    ///
    /// Accepting it binds the new id to the native turn **that steer** opens: the steer's echo
    /// is eaten by the "swallow it while a turn is open" rule, so that turn no longer opens
    /// itself and its deltas and `result` are all recorded under the new id — the web
    /// interface shows a card titled with the new prompt whose body answers the previous
    /// steer, `turn.completed` fires a turn early, and settlement lands on a turn boundary
    /// that is not this RPC's; the real new prompt can then only open another turn id that no
    /// RPC claims.
    #[tokio::test]
    async fn a_turn_start_is_refused_while_a_queued_steer_still_awaits_its_echo() {
        let mut d = probe();
        d.current_turn = Some("turn-1".into());
        d.steer("fix it").await.expect("mid-turn steer is accepted");

        // The turn finishes first and the queued steer is not consumed yet — that is the gap.
        assert!(
            d.classify(json!({"type":"result","is_error":false}))
                .is_some(),
            "the running turn must complete first"
        );
        assert!(d.current_turn.is_none());

        let outcome = d.start_turn("next task").await;
        assert!(
            matches!(
                &outcome,
                super::super::TurnStartOutcome::RetryableNotAccepted { message }
                    if message.contains("has not been echoed back")
            ),
            "a queued steer must hold the next turn back instead of lending it its id, got {outcome:?}"
        );
        assert!(
            d.current_turn.is_none(),
            "a refused start must not mint an id for the queued steer's native turn"
        );
        assert_eq!(
            d.awaiting_echoes
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["fix it"],
            "a refused start must not queue its own echo behind the steer's"
        );

        // The refusal is retryable: the queued message opens its own turn and runs, the queue
        // drains, and the next turn goes through.
        let started = d
            .classify(json!({"type":"user","message":{"role":"user","content":"fix it"}}))
            .expect("the replayed steer opens its own visible turn");
        assert!(
            matches!(
                &started,
                HarnessEvent::TurnStarted { prompt: Some(prompt), .. } if prompt == "fix it"
            ),
            "the queued steer's turn must carry its own prompt, got {started:?}"
        );
        assert!(
            d.classify(json!({"type":"result","is_error":false}))
                .is_some(),
            "the queued steer's turn must close"
        );
        assert!(
            matches!(
                d.start_turn("next task").await,
                super::super::TurnStartOutcome::Accepted { .. }
            ),
            "once the queue has drained the refused turn must go through — the guard must not wedge the session"
        );
        d.shutdown().await.expect("stop test process");
    }

    /// A steer whose stdin write fails withdraws its echo.
    ///
    /// The queue compares against the head only, so an echo that will never arrive sits at the
    /// head forever: every later steer's echo fails to match it, is redrawn as a fresh local
    /// user message inside the same turn, and the queue grows by one every time. A single slot
    /// heals when the next write overwrites it; a queue has no such way out.
    #[tokio::test]
    async fn a_failed_steer_write_does_not_leave_a_stale_echo_at_the_queue_head() {
        let mut d = probe();
        d.current_turn = Some("turn-1".into());
        d.fail_test_write_outcomes(1);

        d.steer("A")
            .await
            .expect_err("the injected write failure must surface");

        // The next steer's echo must still be recognized — that is the real cost of an echo
        // that never comes: "A" stuck at the head makes **every** later echo miss.
        d.steer("B").await.expect("the next steer writes cleanly");
        assert!(
            d.classify(json!({"type":"user","message":{"role":"user","content":"B"}}))
                .is_none(),
            "the next steer's echo was re-rendered as a fresh user message inside the running turn"
        );
        assert!(
            d.awaiting_echoes.is_empty(),
            "a steer whose write failed must not leave an echo that will never come: {:?}",
            d.awaiting_echoes
        );
        d.shutdown().await.expect("stop test process");
    }

    /// Every offered mode must have a native spelling, and reading one back
    /// must land on the same value. A gap here means a picker entry that
    /// silently does nothing, or a "stop asking" button that reports the wrong
    /// guard afterwards.
    #[test]
    fn every_offered_mode_round_trips_through_its_native_spelling() {
        for m in capability().permission_modes {
            let native = native_mode(m);
            assert_eq!(
                neutral_mode(native),
                Some(m),
                "{m:?} → {native} did not come back"
            );
        }
        // Native values with no neutral spelling must not be invented into one.
        assert_eq!(neutral_mode("manual"), None);
        assert_eq!(neutral_mode("dontAsk"), None);
    }

    /// The suggestion shape, verbatim as it arrives on a `can_use_tool` request.
    /// If this stops parsing, "allow and stop asking" silently degrades to a
    /// one-shot allow and the user keeps getting prompted.
    #[test]
    fn the_measured_set_mode_suggestion_is_understood() {
        let measured: Vec<Value> = serde_json::from_str(
            r#"[{"type":"setMode","mode":"acceptEdits","destination":"session"}]"#,
        )
        .unwrap();
        assert_eq!(suggested_mode(&measured), Some(PermissionMode::AcceptEdits));

        // A request with no suggestion must not offer the button.
        assert_eq!(suggested_mode(&[]), None);
        // Nor one whose suggestion we cannot act on.
        let other: Vec<Value> =
            serde_json::from_str(r#"[{"type":"addRule","rule":"Bash(ls:*)"}]"#).unwrap();
        assert_eq!(suggested_mode(&other), None);
        let unknown: Vec<Value> =
            serde_json::from_str(r#"[{"type":"setMode","mode":"dontAsk"}]"#).unwrap();
        assert_eq!(
            suggested_mode(&unknown),
            None,
            "a native mode the daemon cannot account for must disable session scope"
        );
        // Scope likewise: `destination` decides how long this takes effect, see
        // `a_non_session_destination_never_offers_session_scope`.
        let persistent: Vec<Value> = serde_json::from_str(
            r#"[{"type":"setMode","mode":"bypassPermissions","destination":"userSettings"}]"#,
        )
        .unwrap();
        assert_eq!(
            suggested_mode(&persistent),
            None,
            "a destination that outlives the session must disable session scope"
        );
        let no_destination: Vec<Value> =
            serde_json::from_str(r#"[{"type":"setMode","mode":"acceptEdits"}]"#).unwrap();
        assert_eq!(
            suggested_mode(&no_destination),
            None,
            "a missing destination is an unknown lifetime, not an assumed session"
        );
        let mixed: Vec<Value> = serde_json::from_str(
            r#"[{"type":"setMode","mode":"acceptEdits"},{"type":"addRule","rule":"Bash(ls:*)"}]"#,
        )
        .unwrap();
        assert_eq!(
            suggested_mode(&mixed),
            None,
            "all echoed session effects must be modelled before any are offered"
        );
        let two_modes: Vec<Value> = serde_json::from_str(
            r#"[{"type":"setMode","mode":"acceptEdits"},{"type":"setMode","mode":"bypassPermissions"}]"#,
        )
        .unwrap();
        assert_eq!(
            suggested_mode(&two_modes),
            None,
            "multiple effects are not an exact prediction and must fail closed"
        );
    }

    /// Only an authoritative response about this request may release the
    /// daemon's pre-persisted danger arm. Silence, malformed success, and a
    /// success naming another mode keep it armed because native policy changed
    /// or may already have changed.
    #[test]
    fn permission_mode_responses_separate_refusal_from_unknown_outcomes() {
        let response = |subtype: Option<&str>, mode: Option<&str>| {
            let mut response = json!({"request_id": "request-1"});
            if let Some(subtype) = subtype {
                response["subtype"] = json!(subtype);
            }
            if let Some(mode) = mode {
                response["response"] = json!({"mode": mode});
            }
            json!({"type": "control_response", "response": response})
        };

        assert!(
            permission_mode_response(
                &response(Some("success"), Some("bypassPermissions")),
                "request-1",
                PermissionMode::Bypass,
            )
            .expect("matching response")
            .is_ok()
        );

        let explicit = response(Some("error"), None);
        let error = permission_mode_response(&explicit, "request-1", PermissionMode::Bypass)
            .expect("matching response")
            .expect_err("the requested bypass was explicitly not applied");
        assert!(error.is_explicit_refusal());

        for unknown in [
            response(None, None),
            response(Some("success"), None),
            response(Some("success"), Some("auto")),
            response(Some("pending"), None),
            response(Some("future-version"), None),
        ] {
            let error = permission_mode_response(&unknown, "request-1", PermissionMode::Bypass)
                .expect("matching response")
                .expect_err("an incomplete acknowledgement is not confirmation");
            assert!(
                !error.is_explicit_refusal(),
                "unknown outcomes must keep the durable danger arm"
            );
        }

        assert!(
            permission_mode_response(
                &response(Some("error"), None),
                "some-other-request",
                PermissionMode::Bypass,
            )
            .is_none(),
            "another request's refusal proves nothing about ours"
        );
    }

    /// `dangerous` and an explicit mode are two spellings of the same decision
    /// and must not be able to disagree — whichever says "no checks" wins.
    #[test]
    fn the_two_ways_of_asking_for_no_checks_agree() {
        let spec = |dangerous, permission_mode| LaunchSpec {
            cwd: PathBuf::from("/tmp"),
            resume_from: None,
            agit_session: None,
            model: None,
            dangerous,
            permission_mode,
        };
        assert_eq!(spec(true, None).effective_mode(), PermissionMode::Bypass);
        assert_eq!(
            spec(false, Some(PermissionMode::Bypass)).effective_mode(),
            PermissionMode::Bypass
        );
        // An explicit `dangerous` still counts: the two spellings say the same thing and
        // whichever says "no checks" wins — the safe direction must not be quietly loosened
        // from either side.
        assert_eq!(
            spec(true, Some(PermissionMode::Plan)).effective_mode(),
            PermissionMode::Bypass,
            "an explicit `dangerous` still means dangerous"
        );
        assert_eq!(spec(false, None).effective_mode(), PermissionMode::Default);
    }

    #[test]
    fn capability_reports_the_measured_steer_semantics() {
        let c = capability();
        assert_eq!(
            c.steer,
            Some(Delivery::AtToolBoundary),
            "measured: 31s, delivered at the tool boundary"
        );
        assert!(c.interrupt && c.approvals && c.partial_messages);
    }
}
