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

mod commands;
pub(crate) mod program;

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
/// repository's command names and descriptions ride along with machine discovery to the viewers of
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

pub(crate) fn version_of(program: &program::Program) -> Option<String> {
    let mut command = crate::infra::background::command(&program.path);
    command.arg("--version");
    if let Some((name, value)) = program.env() {
        command.env(name, value);
    }
    let output = command.output().ok()?;
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !version.is_empty()).then_some(version)
}

pub fn capability() -> RuntimeCapability {
    let program = program::resolve();
    let (commands, commands_omitted) = super::bounded_commands(cached_machine_commands());
    RuntimeCapability {
        runtime: "claude-code".into(),
        available: program.is_some(),
        version: program.as_ref().and_then(version_of),
        steer: Some(Delivery::AtToolBoundary),
        interrupt: true,
        approvals: true,
        partial_messages: true,
        resume: true,
        // Discovery reads cached commands because the harness provides its catalog only
        // after session initialization. Project commands must stay out of this shared view.
        commands,
        commands_omitted,
        // All five map onto native `--permission-mode` values, and the switch
        // is live: a `set_permission_mode` control request is answered
        // `{"mode":"auto"}` and the very next `system/init` reports the new
        // mode.
        permission_modes: [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
            PermissionMode::Plan,
            PermissionMode::Bypass,
        ]
        .into_iter()
        .filter(|mode| !(mode.is_dangerous() && bypass_refused()))
        .collect(),
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
    question_input: Option<Value>,
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
    /// An interrupt echo precedes its terminal result. Do not admit new input
    /// until that result drains or it can close the next turn by mistake.
    interrupt_draining: bool,
    stream_items: std::collections::HashMap<u64, String>,
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
    model: Option<String>,
    effort: Option<String>,
    effort_known: bool,
    model_choice: Option<String>,
    model_catalog: Vec<Value>,
    pending_model_request: Option<(String, Value)>,
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
    /// Each written message awaiting its echo, with whether it is the owner's words.
    awaiting_echoes: std::collections::VecDeque<(String, bool)>,
    pub(super) next_prompt_id: Option<String>,
    goal_query_pending: bool,
    /// The one turn known to hold only the owner's words, by its id. Only that turn may start
    /// work that continues after it; any other (a turn Claude Code opened by itself, one a
    /// non-owner wrote into, or none at all) can't, so a turn we can't attribute fails closed.
    owner_turn: Option<String>,
    /// Whose words the next `start_turn` or `steer` writes, set just before it.
    next_input_owner: Option<bool>,
}

/// Every call passes through a PreToolUse hook (`answer_hook`), because Claude Code asks no
/// permission for some calls that start work continuing after the turn, and any tool may take
/// a background flag.
const OUTLIVES_TURN_MATCHER: &str = "*";
const OUTLIVES_TURN_HOOK: &str = "agit-outlives-turn";
/// What the model reads when such a call is refused. It states the rule and nothing more.
pub(crate) const OUTLIVES_TURN_REFUSAL: &str = "Only this conversation's owner can start work that continues after this turn; nothing was started.";

/// The tools a turn that isn't the owner's alone may call, each still subject to Claude Code's
/// own permission check: they act inside the turn and end with it. Every other tool, one Claude
/// Code adds later and every MCP tool included, is refused in such a turn.
const NON_OWNER_TOOLS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    "Bash",
    "WebFetch",
    "WebSearch",
    "TodoWrite",
    "ToolSearch",
    "ExitPlanMode",
    "AskUserQuestion",
];

/// Whether a turn that isn't the owner's alone is refused this call: any tool not on
/// [`NON_OWNER_TOOLS`], and any call asking to run in the background. A call it can't read is
/// refused.
fn outlives_turn(tool: Option<&str>, input: &Value) -> bool {
    match tool {
        Some(name) if NON_OWNER_TOOLS.contains(&name) => {
            !input.is_object()
                || !matches!(
                    input.get("run_in_background"),
                    None | Some(Value::Null) | Some(Value::Bool(false))
                )
        }
        _ => true,
    }
}

/// Settings every session agit drives runs with: Claude Code never moves a running call to the
/// background (after a timeout, to deliver a queued message, or on its own), so no turn leaves
/// a command running after it. A session can't switch this per turn, so it holds for the
/// owner's turns too. They are passed twice: in the process environment, and as `--settings`
/// on the command line, because an `env` block in user or project settings overrides the
/// process environment and the command line ranks above both.
const SESSION_ENV: &[(&str, &str)] = &[
    ("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", "1"),
    ("CLAUDE_AUTO_BACKGROUND_TASKS", "0"),
];

/// The environment `launch` adds for the session, minus the program's own.
fn session_env(spec: &LaunchSpec) -> Vec<(String, String)> {
    let mut env = super::lineage_env(spec.agit_session.as_ref());
    // Mark the child so nested tooling can tell it is running under RC.
    env.push(("AGIT_RC".into(), "1".into()));
    env.extend(
        SESSION_ENV
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into())),
    );
    env
}

/// The exact command line `launch` runs, minus the spawn.
///
/// Split out so a test can assert against **the argv that actually runs** — a copy
/// transcribed into the test only ever pins the copy.
fn argv(spec: &LaunchSpec, session_id: &str, resume: bool) -> Vec<String> {
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
    args.push("--settings".into());
    args.push(session_settings().to_string());
    args.push(if resume { "--resume" } else { "--session-id" }.into());
    args.push(session_id.to_string());
    args
}

/// The `--settings` value every session runs with: [`SESSION_ENV`] as an `env` block.
fn session_settings() -> Value {
    let env: serde_json::Map<String, Value> = SESSION_ENV
        .iter()
        .map(|(key, value)| ((*key).into(), json!(value)))
        .collect();
    json!({ "env": env })
}

/// A reopened conversation whose transcript Claude no longer has, while its archive holds turns.
///
/// Claude deletes old transcripts on its own schedule, and a conversation opened but never sent a
/// message has none yet. `--resume` of either exits at once, so the launch is decided before the
/// spawn. Starting over under the same id is refused here: settlement would reject a transcript
/// that no longer continues the archived turns.
#[derive(Debug, thiserror::Error)]
#[error(
    "Claude Code no longer has this conversation's transcript; its archived turns can be continued with `agit resume`"
)]
pub(crate) struct NativeTranscriptMissing;

/// Whether the archive of a conversation already holds a settled transcript.
fn archived_turns(lineage: &crate::rc::lineage::AgitSession) -> bool {
    let Some(repo) = lineage
        .repo_dir()
        .ok()
        .and_then(crate::domain::repo::Repo::open)
    else {
        return false;
    };
    repo.git_opt(&[
        "cat-file",
        "-e",
        &format!(
            "refs/heads/{}:{}",
            lineage.branch(),
            crate::domain::meta::LOG_FILE
        ),
    ])
    .is_some()
}

/// Claude exits at once on `--dangerously-skip-permissions` when it runs as root outside a sandbox
/// it recognizes, so on such a machine the mode is never offered or launched.
fn bypass_refused() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions.
        let root = unsafe { libc::getuid() } == 0;
        let bubblewrap = std::env::var("CLAUDE_CODE_BUBBLEWRAP").is_ok_and(|value| {
            !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
        });
        root && std::env::var("IS_SANDBOX").as_deref() != Ok("1") && !bubblewrap
    }
    #[cfg(not(unix))]
    false
}

/// A new conversation without permission checks cannot start where Claude refuses them.
#[derive(Debug, thiserror::Error)]
#[error(
    "Claude Code refuses to skip permission checks as root; choose another permission mode, or run the daemon in a sandbox Claude recognizes"
)]
pub(crate) struct BypassUnavailable;

/// Fits the requested mode to what Claude accepts here. A reopened conversation recorded without
/// permission checks continues with approvals instead of failing every launch.
fn admit_mode(spec: &mut LaunchSpec, bypass_refused: bool) -> Result<(), LaunchError> {
    if !(bypass_refused && spec.effective_mode().is_dangerous()) {
        return Ok(());
    }
    if spec.resume_from.is_none() {
        return Err(LaunchError::not_spawned(BypassUnavailable.into()));
    }
    spec.dangerous = false;
    spec.permission_mode = Some(PermissionMode::Default);
    Ok(())
}

/// Whether a reopened conversation is resumed from its transcript or continued under its own id.
fn reopen(spec: &LaunchSpec) -> Result<bool, LaunchError> {
    let Some(id) = spec.resume_from.as_deref() else {
        return Ok(false);
    };
    if crate::adapter::claude_code::locate_transcript(id, Some(&spec.cwd)).is_some() {
        return Ok(true);
    }
    if spec.agit_session.as_ref().is_some_and(archived_turns) {
        return Err(LaunchError::not_spawned(NativeTranscriptMissing.into()));
    }
    // Nothing was settled, so the conversation continues under its own id.
    Ok(false)
}

impl ClaudeCodeDriver {
    /// Start a claude-code session. Failure comes back as [`LaunchError`], propagating
    /// "did the OS spawn happen" unchanged: an error from `Proc::spawn` is **provably nothing
    /// started**, while a failed handshake write leaves a process already running.
    pub async fn launch(mut spec: LaunchSpec) -> Result<ClaudeCodeDriver, LaunchError> {
        admit_mode(&mut spec, bypass_refused())?;
        let resume = reopen(&spec)?;
        let session_id = spec
            .resume_from
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

        let args = argv(&spec, &session_id, resume);
        let mode = spec.effective_mode();

        let mut env = session_env(&spec);

        let program = program::resolve().ok_or_else(|| {
            LaunchError::not_spawned(anyhow::anyhow!(
                "Claude Code is not installed where this device's daemon can run it: no `claude` \
                 on the daemon's PATH or in Claude Code's install directories"
            ))
        })?;
        env.extend(program.env());
        let mut proc = Proc::spawn(&program.path.to_string_lossy(), &args, &spec.cwd, &env)?;

        // The SDK handshake. Declaring it is what puts the CLI in "ask the
        // client" mode for permissions; without it we would silently fall back
        // to whatever the local settings.json says, which is exactly the
        // failure mode where a shared workspace quietly self-approves.
        proc.write_line(&json!({
            "type": "control_request",
            "request_id": "agit-init",
            "request": { "subtype": "initialize", "hooks": {
                "PreToolUse": [{
                    "matcher": OUTLIVES_TURN_MATCHER,
                    "hookCallbackIds": [OUTLIVES_TURN_HOOK],
                }],
            } }
        }))
        .await
        .map_err(LaunchError::spawned)?;

        let model_choice = spec
            .model
            .clone()
            .or_else(|| spec.resume_from.is_none().then(|| "default".into()));
        Ok(ClaudeCodeDriver {
            proc,
            session_id,
            cwd: spec.cwd,
            current_turn: None,
            interrupt_draining: false,
            stream_items: Default::default(),
            pending_approvals: Default::default(),
            mode,
            model: spec.model,
            effort: None,
            effort_known: false,
            model_choice,
            model_catalog: vec![],
            pending_model_request: None,
            ready_sent: false,
            commands: vec![],
            pushback: Default::default(),
            awaiting_echoes: Default::default(),
            next_prompt_id: None,
            goal_query_pending: false,
            owner_turn: None,
            next_input_owner: None,
        })
    }

    /// The `claude -p` process this driver runs, while it has not been reaped.
    pub fn harness_pid(&self) -> Option<u32> {
        self.proc.pid()
    }

    pub fn runtime_thread_id(&self) -> Option<&str> {
        Some(&self.session_id)
    }

    /// Use the adapter's configuration root so live output and settlement
    /// resolve the same transcript, including custom CLAUDE_CONFIG_DIR values.
    pub fn transcript_path(&self) -> Option<PathBuf> {
        Some(
            crate::adapter::claude_code::projects_dir()
                .ok()?
                .join(crate::adapter::claude_code::slug_for(&self.cwd))
                .join(format!("{}.jsonl", self.session_id)),
        )
    }

    pub async fn start_turn(&mut self, message: &str) -> super::TurnStartOutcome {
        let owner = self.next_input_owner.take() == Some(true);
        if self.goal_query_pending {
            return super::TurnStartOutcome::RetryableNotAccepted {
                message: "Claude goal recovery is incomplete; restart the local daemon".into(),
            };
        }
        if self.interrupt_draining {
            return super::TurnStartOutcome::RetryableNotAccepted {
                message:
                    "the interrupted Claude turn is still closing; retry after its result drains"
                        .into(),
            };
        }
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
        self.owner_turn = owner.then(|| turn.clone());
        self.awaiting_echoes.push_back((message.to_string(), owner));
        let prompt_id = self
            .next_prompt_id
            .take()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        match self
            .proc
            .write_line(&json!({"type":"user","uuid":prompt_id,"message":{"role":"user","content":message}}))
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
        let owner = self.next_input_owner.take() == Some(true);
        if self.current_turn.is_none() {
            return Err(super::TurnNotRunning.into());
        }
        // Someone else's words now share this turn.
        if !owner {
            self.owner_turn = None;
        }
        self.awaiting_echoes.push_back((message.to_string(), owner));
        let prompt_id = self
            .next_prompt_id
            .take()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if let Err(error) = self
            .proc
            .write_line(&json!({"type":"user","uuid":prompt_id,"message":{"role":"user","content":message}}))
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

    /// Interrupt the running turn. A turn-fenced interrupt (one naming the turn to stop) is
    /// refused and nothing is written: Claude Code starts turns of its own (a background task's
    /// end, a Monitor event, a scheduled prompt, a steer that opens the next turn), some without
    /// asking first, and the turn this driver has read may be behind the one running, so an
    /// interrupt could stop a turn the caller did not name. Without a fence, whatever runs is
    /// interrupted. `interrupt_fenced()` still reports false for this runtime: the Hub reads true
    /// there as a terminal-attached conversation.
    pub async fn interrupt_fenced(
        &mut self,
        expected_turn_id: Option<&str>,
    ) -> crate::Result<super::InterruptOutcome> {
        if expected_turn_id.is_some() {
            return Err(super::FenceUnsupported.into());
        }
        self.interrupt().await?;
        Ok(super::InterruptOutcome::Requested)
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
                if let Some(mut input) = pending.question_input.clone() {
                    let answers = r.answers.clone().unwrap_or_default();
                    input["answers"] = json!(
                        answers
                            .into_iter()
                            .map(|(key, value)| (key, value.join(", ")))
                            .collect::<std::collections::BTreeMap<_, _>>()
                    );
                    body["updatedInput"] = input;
                }
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

    async fn model_request(&mut self, request: Value) -> crate::Result<()> {
        let id = uuid::Uuid::new_v4().to_string();
        self.pending_model_request = Some((id.clone(), request.clone()));
        self.proc
            .write_line(&json!({"type":"control_request","request_id":id,"request":request}))
            .await?;
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(line) = self.proc.next().await {
                if let Line::Json(v) = line.line()
                    && v.pointer("/response/request_id").and_then(Value::as_str)
                        == Some(id.as_str())
                {
                    self.capture_model_response(v);
                    anyhow::ensure!(
                        v.pointer("/response/subtype").and_then(Value::as_str) == Some("success"),
                        "Claude refused model settings: {}",
                        v["response"]
                    );
                    return Ok(());
                }
                self.pushback.push(line);
            }
            anyhow::bail!("Claude exited before confirming model settings")
        })
        .await;
        match result {
            Ok(result) => result,
            Err(_) => {
                anyhow::bail!("Model settings outcome is unknown; waiting for Claude confirmation")
            }
        }
    }

    fn capture_model_response(&mut self, response: &Value) {
        let Some((id, _)) = &self.pending_model_request else {
            return;
        };
        if response
            .pointer("/response/request_id")
            .and_then(Value::as_str)
            != Some(id.as_str())
        {
            return;
        }
        let (_, request) = self
            .pending_model_request
            .take()
            .expect("matched settings request");
        if response
            .pointer("/response/subtype")
            .and_then(Value::as_str)
            != Some("success")
        {
            return;
        }
        if request["subtype"] == "set_model" {
            self.model = request["model"].as_str().map(String::from);
            self.model_choice = Some(self.model.clone().unwrap_or_else(|| "default".into()));
        } else if request["subtype"] == "apply_flag_settings" {
            self.effort = request["settings"]["effortLevel"]
                .as_str()
                .map(String::from);
            self.effort_known = true;
        }
    }

    pub async fn model_control(
        &mut self,
        patch: Option<&super::models::ModelPatch>,
    ) -> crate::Result<Value> {
        if self.model_catalog.is_empty() {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while let Some(line) = self.proc.next().await {
                    let found = if let Line::Json(value) = line.line() {
                        let native = &value["response"]["response"];
                        if let Ok(models) = super::models::normalize("claude-code", native) {
                            self.model_catalog = models;
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    self.pushback.push(line);
                    if found {
                        break;
                    }
                }
            })
            .await
            .map_err(|_| anyhow::anyhow!("Claude is still opening its model catalog"))?;
        }
        if let Some(patch) = patch {
            anyhow::ensure!(
                self.pending_model_request.is_none(),
                "A native settings change is still awaiting confirmation"
            );
            anyhow::ensure!(
                self.current_turn.is_none() && !self.interrupt_draining,
                "Wait for the current turn before changing model settings"
            );
            let desired = patch
                .model
                .as_ref()
                .map(|m| m.as_deref())
                .unwrap_or(self.model_choice.as_deref().or(self.model.as_deref()));
            let selected = super::models::selected(&self.model_catalog, desired);
            // A model is named only from this session's catalog, whoever asks: an id it doesn't
            // list is refused here, and the catalog's own id is what Claude Code is sent.
            let model = match &patch.model {
                Some(Some(_)) => Some(Some(
                    selected
                        .and_then(|entry| entry["id"].as_str())
                        .ok_or_else(|| {
                            anyhow::anyhow!("This model is absent from the native catalog")
                        })?
                        .to_string(),
                )),
                Some(None) => Some(None),
                None => None,
            };
            let supports_effort = super::models::efforts(selected)
                .as_array()
                .is_some_and(|values| !values.is_empty());
            if let Some(Some(effort)) = &patch.effort {
                super::models::check_effort(selected, effort)?;
            }
            if let Some(model) = &model {
                self.model_request(json!({"subtype":"set_model","model":model}))
                    .await?;
            }
            // A model switch clears the override so an old model's effort cannot leak into a new one.
            let effort = patch.effort.as_ref().cloned().or_else(|| {
                patch
                    .model
                    .as_ref()
                    .and_then(|_| (supports_effort || self.effort_known).then_some(None))
            });
            if let Some(effort) = effort {
                self.model_request(
                    json!({"subtype":"apply_flag_settings","settings":{"effortLevel":effort}}),
                )
                .await?;
            }
        }
        let efforts = super::models::efforts(super::models::selected(
            &self.model_catalog,
            self.model_choice.as_deref().or(self.model.as_deref()),
        ));
        Ok(
            json!({"model":self.model,"selected_model":self.model_choice,"effort":self.effort,"effort_known":self.effort_known && self.pending_model_request.is_none(),"pending":null,"settings_unknown":self.pending_model_request.is_some(),
            "models":self.model_catalog,"efforts":efforts,"applied":"immediate",
            "capabilities":{"model":self.pending_model_request.is_none(),"effort":self.pending_model_request.is_none() && efforts.as_array().is_some_and(|v| !v.is_empty()),"reset_model":true,"reset_effort":true}}),
        )
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
            if v["type"] == "control_request" && v["request"]["subtype"] == "hook_callback" {
                if let Err(error) = self.answer_hook(&v).await {
                    // Claude Code waits on this answer before running the call; one that may not
                    // have arrived leaves nothing safe to continue on.
                    return Some(HarnessEvent::ProtocolInvariant {
                        message: format!("claude hook answer was not delivered: {error}"),
                        attempted_mode: None,
                        confirmation_token: None,
                    });
                }
                continue;
            }
            if let Some(ev) = self.classify(v) {
                return Some(ev);
            }
        }
    }

    /// Whose words the next `start_turn` or `steer` writes.
    pub(crate) fn note_input_owner(&mut self, owner: bool) {
        self.next_input_owner = Some(owner);
    }

    /// Answer a PreToolUse hook: refuse a call that would start work continuing after the turn
    /// unless the open turn holds only the owner's words; say nothing about any other call, so
    /// it goes on to Claude Code's own permission check.
    async fn answer_hook(&mut self, v: &Value) -> crate::Result<()> {
        let request_id = v["request_id"].as_str().unwrap_or_default();
        let input = &v["request"]["input"];
        let owners = self.current_turn.is_some() && self.current_turn == self.owner_turn;
        let body = if v["request"]["callback_id"] == OUTLIVES_TURN_HOOK
            && !owners
            && outlives_turn(input["tool_name"].as_str(), &input["tool_input"])
        {
            json!({"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": OUTLIVES_TURN_REFUSAL,
            }})
        } else {
            json!({})
        };
        self.proc
            .write_line(&json!({
                "type": "control_response",
                "response": {"subtype": "success", "request_id": request_id, "response": body},
            }))
            .await
    }

    /// Map one stream-json frame to a harness event. `None` means "internal,
    /// keep reading" — heartbeats, token counters, and the like.
    fn classify(&mut self, v: Value) -> Option<HarnessEvent> {
        let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        match ty {
            "assistant" if v.get("local_command_source").is_some() => {
                let text = v["message"]["content"]
                    .as_array()?
                    .iter()
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                commands::goal_from_text(&text).map(|goal| HarnessEvent::GoalUpdated { goal })
            }
            "system" => {
                let sub = v.get("subtype").and_then(|x| x.as_str()).unwrap_or("");
                if sub == "api_retry" {
                    let attempt = v.get("attempt").and_then(Value::as_u64).unwrap_or(0);
                    let seconds = v
                        .get("retry_delay_ms")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0)
                        / 1000.0;
                    return Some(HarnessEvent::Progress {
                        text: format!(
                            "Retrying model connection (attempt {attempt}, {:.0}s)",
                            seconds.ceil()
                        ),
                    });
                }
                if sub == "status" && v.get("status").and_then(Value::as_str) == Some("compacting")
                {
                    return Some(HarnessEvent::Progress {
                        text: "Compacting conversation".into(),
                    });
                }
                if sub == "init" {
                    self.model = v
                        .get("model")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or(self.model.clone());
                }
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
                self.capture_model_response(&v);
                let r = v.get("response")?.get("response")?;
                if let Ok(models) = super::models::normalize("claude-code", r) {
                    self.model_catalog = models;
                }
                let arr = r.get("commands")?.as_array()?;
                let mut harvested: Vec<crate::protocol::SlashCommand> = arr
                    .iter()
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
                    .collect();
                for command in arr {
                    for alias in command["aliases"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                    {
                        if !harvested.iter().any(|entry| entry.name == alias) {
                            harvested.push(crate::protocol::SlashCommand {
                                name: alias.to_string(),
                                description: command["description"].as_str().map(String::from),
                                argument_hint: command["argumentHint"]
                                    .as_str()
                                    .filter(|value| !value.is_empty())
                                    .map(String::from),
                            });
                        }
                    }
                }
                // Cache only machine-scoped commands for discovery outside this session.
                // A failed cache write does not invalidate the session's live catalog.
                if !harvested.is_empty() && harvested != self.commands {
                    let _ =
                        crate::rc::save_json(COMMAND_CACHE, &machine_commands(harvested.clone()));
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
                        question_input: (tool == "AskUserQuestion").then(|| input.clone()),
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
                        let id = cb
                            .get("id")
                            .and_then(Value::as_str)
                            .map(String::from)
                            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                        self.stream_items.insert(idx, id);
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
                // Native control acknowledgements use replayed user-shaped records,
                // but they have no model result and cannot open a turn. A matching
                // submitted prompt still owns its echo even if it contains this markup.
                if v.get("isReplay").and_then(Value::as_bool) == Some(true)
                    && text.starts_with("<local-command-stdout>")
                    && self.awaiting_echoes.front().map(|(echo, _)| echo) != Some(&text)
                {
                    return None;
                }
                if text.starts_with("[Request interrupted") {
                    // No open turn = this interrupt closed nothing (the turn ended on its
                    // own and the interrupt's echo arrived a step late). Sending
                    // `turn.completed` here is **forbidden**: see the ghost-turn argument in
                    // the `result` branch below.
                    let turn = self.current_turn.take()?;
                    self.interrupt_draining = true;
                    return Some(HarnessEvent::TurnCompleted {
                        turn_id: turn,
                        outcome: TurnOutcome::Interrupted,
                        error: None,
                        cost_usd: None,
                        duration_ms: None,
                    });
                }
                // Our own message coming back through --replay-user-messages.
                // Echoes come back in write order, so only the head is consumed; a match
                // deeper in the queue means the order broke, and it is better to surface it
                // as a new message.
                let echoed = self.awaiting_echoes.front().map(|(echo, _)| echo) == Some(&text);
                // Whose words these are travels with the message, not with the turn it was
                // written into: a queued message can open the next turn. Words not matched to a
                // written message are no one's.
                let owners_words = echoed
                    && self
                        .awaiting_echoes
                        .pop_front()
                        .is_some_and(|(_, owner)| owner);
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
                    if !owners_words {
                        self.owner_turn = None;
                    }
                    return None;
                }
                let opened = self.current_turn.is_none();
                let turn = self
                    .current_turn
                    .get_or_insert_with(|| uuid::Uuid::new_v4().to_string())
                    .clone();
                if opened {
                    // Messages still queued join this turn when they echo; it is the owner's
                    // only if all of them are too.
                    let owners =
                        owners_words && self.awaiting_echoes.iter().all(|(_, owner)| *owner);
                    self.owner_turn = owners.then(|| turn.clone());
                } else if !owners_words {
                    self.owner_turn = None;
                }
                Some(HarnessEvent::TurnStarted {
                    turn_id: turn,
                    prompt: Some(text),
                })
            }

            "result" if self.goal_query_pending && v["local_command"] == "goal" => {
                self.goal_query_pending = false;
                commands::goal_from_text(v["result"].as_str().unwrap_or_default())
                    .map(|goal| HarnessEvent::GoalUpdated { goal })
            }
            "result" => {
                if self.interrupt_draining {
                    self.interrupt_draining = false;
                    return None;
                }
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
                let error = is_err.then(|| {
                    let errors = v
                        .get("errors")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|message| !message.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !errors.is_empty() {
                        errors
                    } else {
                        v.get("result")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|message| !message.is_empty())
                            .unwrap_or("Claude Code ended the turn with an error.")
                            .to_owned()
                    }
                });
                Some(HarnessEvent::TurnCompleted {
                    turn_id: turn,
                    outcome: if is_err {
                        TurnOutcome::Error
                    } else {
                        TurnOutcome::Ok
                    },
                    error,
                    cost_usd: v.get("total_cost_usd").and_then(|x| x.as_f64()),
                    duration_ms: v.get("duration_api_ms").and_then(|x| x.as_u64()),
                })
            }

            _ => None,
        }
    }

    /// Block index → a stable item id within the current turn.
    fn item_id(&self, index: u64) -> String {
        self.stream_items
            .get(&index)
            .cloned()
            .unwrap_or_else(|| format!("{}:{index}", self.current_turn.as_deref().unwrap_or("t")))
    }

    pub async fn shutdown(&mut self) -> crate::Result<()> {
        self.proc.shutdown().await
    }

    /// The turn that had not reached its `result` when the harness stopped.
    pub(crate) fn current_turn(&self) -> Option<&str> {
        self.current_turn.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn test_driver() -> ClaudeCodeDriver {
        Self::test_driver_on(
            Proc::spawn("cat", &[], &PathBuf::from("/"), &[]).expect("test process"),
        )
    }

    /// A test driver whose every line written to Claude Code is appended to `path`, and nothing
    /// comes back, so a supervisor running it cannot consume what it wrote.
    #[cfg(all(test, unix))]
    pub(crate) fn test_driver_recording(path: &std::path::Path) -> ClaudeCodeDriver {
        Self::test_driver_on(
            Proc::spawn(
                "sh",
                &["-c".into(), format!("cat >> '{}'", path.display())],
                &PathBuf::from("/"),
                &[],
            )
            .expect("test process"),
        )
    }

    #[cfg(all(test, unix))]
    pub(crate) fn set_test_mode(&mut self, mode: PermissionMode) {
        self.mode = mode;
    }

    #[cfg(test)]
    fn test_driver_on(proc: Proc) -> ClaudeCodeDriver {
        ClaudeCodeDriver {
            proc,
            session_id: "test-session".into(),
            cwd: PathBuf::from("/"),
            current_turn: Some("test-turn".into()),
            interrupt_draining: false,
            stream_items: Default::default(),
            pending_approvals: Default::default(),
            model: None,
            effort: None,
            effort_known: false,
            model_choice: None,
            model_catalog: vec![],
            pending_model_request: None,
            mode: PermissionMode::Default,
            ready_sent: true,
            commands: vec![],
            pushback: Default::default(),
            awaiting_echoes: Default::default(),
            next_prompt_id: None,
            goal_query_pending: false,
            owner_turn: None,
            next_input_owner: None,
        }
    }

    /// A test driver whose Claude Code is `sh -c script`, with no turn open.
    #[cfg(test)]
    pub(crate) fn test_driver_running(script: &str) -> ClaudeCodeDriver {
        let mut driver = Self::test_driver();
        driver.proc = Proc::spawn(
            "sh",
            &["-c".into(), script.into()],
            &PathBuf::from("/"),
            &[],
        )
        .expect("test process");
        driver.current_turn = None;
        driver
    }

    /// A `sh` stand-in for Claude Code that appends every line written to it to `written` and,
    /// for each prompt (and once at start when `unprompted`), asks the PreToolUse hook about a
    /// call to `tool` with `input`.
    #[cfg(test)]
    pub(crate) fn hook_stand_in(
        directory: &std::path::Path,
        tool: &str,
        input: &Value,
        unprompted: bool,
    ) -> (String, PathBuf) {
        let written = directory.join("written.jsonl");
        let hook = directory.join("hook.json");
        std::fs::write(
            &hook,
            json!({"type":"control_request","request_id":"hook-1","request":{
                "subtype":"hook_callback","callback_id":OUTLIVES_TURN_HOOK,"input":{
                    "hook_event_name":"PreToolUse","tool_name":tool,"tool_input":input}}})
            .to_string()
                + "\n",
        )
        .unwrap();
        let script = format!(
            "{}while IFS= read -r l; do printf '%s\\n' \"$l\" >> '{}'; case \"$l\" in *'\"type\":\"user\"'*) cat '{}';; esac; done",
            if unprompted {
                format!("cat '{}'; ", hook.display())
            } else {
                String::new()
            },
            written.display(),
            hook.display(),
        );
        (script, written)
    }

    /// The PreToolUse hook answers a stand-in recorded, in order.
    #[cfg(test)]
    pub(crate) fn hook_responses(path: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line["type"] == "control_response")
            .map(|line| line["response"]["response"].clone())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn clear_test_current_turn(&mut self) {
        self.current_turn = None;
    }

    #[cfg(test)]
    pub(crate) fn set_test_current_turn(&mut self, turn: &str) {
        self.current_turn = Some(turn.into());
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
                question_input: None,
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

    /// Drives a Claude Code stand-in that asks the PreToolUse hook about one `tool` call: at
    /// start when `unprompted` (a turn nobody here opened), and for every prompt written to it.
    /// Each write is `(steer, owner)`. Returns what the driver answered, in order.
    async fn hook_answers(
        tool: &str,
        input: Value,
        unprompted: bool,
        writes: &[(bool, bool)],
    ) -> Vec<Value> {
        let directory = tempfile::tempdir().unwrap();
        let (script, written) =
            ClaudeCodeDriver::hook_stand_in(directory.path(), tool, &input, unprompted);
        let mut driver = ClaudeCodeDriver::test_driver_running(&script);
        let mut expected = usize::from(unprompted);
        settle(&mut driver, &written, expected).await;
        for (steer, owner) in writes {
            driver.note_input_owner(*owner);
            if *steer {
                driver.steer("more").await.unwrap();
            } else {
                assert!(matches!(
                    driver.start_turn("go").await,
                    super::super::TurnStartOutcome::Accepted { .. }
                ));
            }
            expected += 1;
            settle(&mut driver, &written, expected).await;
        }
        let _ = driver.shutdown().await;
        ClaudeCodeDriver::hook_responses(&written)
    }

    /// Reads the stand-in's frames until the driver has written `expected` hook answers.
    async fn settle(driver: &mut ClaudeCodeDriver, written: &std::path::Path, expected: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while ClaudeCodeDriver::hook_responses(written).len() < expected {
            assert!(
                std::time::Instant::now() < deadline,
                "the hook was never answered"
            );
            let _ = tokio::time::timeout(std::time::Duration::from_millis(50), driver.next_event())
                .await;
        }
    }

    fn refused(answer: &Value) -> bool {
        answer["hookSpecificOutput"]["permissionDecision"] == "deny"
            && answer["hookSpecificOutput"]["permissionDecisionReason"] == OUTLIVES_TURN_REFUSAL
    }

    /// Asserts that a non-owner's turn is refused this call with the fixed text, and the owner's
    /// turn is left to Claude Code (an empty answer, not an allow that would skip its own check).
    async fn held_to_the_owner(tool: &str, input: Value) {
        let non_owner = hook_answers(tool, input.clone(), false, &[(false, false)]).await;
        assert_eq!(non_owner.len(), 1, "{tool}");
        assert!(refused(&non_owner[0]), "{tool}: {non_owner:?}");
        let owner = hook_answers(tool, input, false, &[(false, true)]).await;
        assert_eq!(owner, vec![json!({})], "{tool}");
    }

    /// Pins the rule for one call that outlives the turn, kept in this session or on disk: a
    /// non-owner's turn is refused it, the owner's is not.
    #[tokio::test]
    async fn a_non_owners_turn_cannot_schedule_a_prompt() {
        for durable in [false, true] {
            held_to_the_owner(
                "CronCreate",
                json!({"cron":"* * * * *","prompt":"later","recurring":false,"durable":durable}),
            )
            .await;
        }
    }

    /// Pins the rule for another call that outlives the turn.
    #[tokio::test]
    async fn a_non_owners_turn_cannot_schedule_a_wakeup() {
        held_to_the_owner(
            "ScheduleWakeup",
            json!({"delaySeconds":60,"reason":"later","prompt":"later"}),
        )
        .await;
    }

    /// Pins the rule for another call that outlives the turn.
    #[tokio::test]
    async fn a_non_owners_turn_cannot_start_a_monitor() {
        held_to_the_owner("Monitor", json!({"command":"sleep 4","timeout_ms":30000})).await;
    }

    /// Pins the rule for a command that outlives the turn, and that one which ends inside the turn
    /// is left to Claude Code's own permission check.
    #[tokio::test]
    async fn a_non_owners_turn_cannot_start_a_background_command() {
        held_to_the_owner(
            "Bash",
            json!({"command":"sleep 6","run_in_background":true}),
        )
        .await;
        for input in [
            json!({"command":"ls"}),
            json!({"command":"ls","run_in_background":false}),
        ] {
            let answers = hook_answers("Bash", input, false, &[(false, false)]).await;
            assert_eq!(answers, vec![json!({})]);
        }
    }

    /// Pins that a call of any tool asking to run in the background is refused a non-owner's
    /// turn, not only the tools named for it.
    #[tokio::test]
    async fn a_non_owners_turn_cannot_start_a_background_agent() {
        held_to_the_owner(
            "Agent",
            json!({"description":"later","prompt":"later","run_in_background":true}),
        )
        .await;
        held_to_the_owner("SomeFutureTool", json!({"run_in_background":"yes"})).await;
        let answers =
            hook_answers("Read", json!({"file_path":"a"}), false, &[(false, false)]).await;
        assert_eq!(answers, vec![json!({})]);
    }

    /// Pins the rule for the calls that start other agents' work, which may go on after the turn
    /// without asking to run in the background, and for those that cancel or stop work running
    /// past it.
    #[tokio::test]
    async fn a_non_owners_turn_cannot_start_another_agent() {
        for tool in [
            "Agent",
            "Task",
            "Workflow",
            "SendMessage",
            "Skill",
            "CronDelete",
            "TaskStop",
        ] {
            held_to_the_owner(tool, json!({"prompt":"now"})).await;
        }
    }

    /// The tools Claude Code 2.1.311 lists in its stream-json `system/init` for a headless
    /// session (captured from the real binary), plus tools it offers in other configurations.
    const CLAUDE_CODE_TOOLS: &[&str] = &[
        "Task",
        "AskUserQuestion",
        "Bash",
        "CronCreate",
        "CronDelete",
        "CronList",
        "Edit",
        "EnterPlanMode",
        "EnterWorktree",
        "ExitPlanMode",
        "ExitWorktree",
        "ListAgents",
        "Monitor",
        "NotebookEdit",
        "Read",
        "ReportFindings",
        "ScheduleWakeup",
        "SendMessage",
        "Skill",
        "TaskStop",
        "ToolSearch",
        "WebFetch",
        "WebSearch",
        "Workflow",
        "Write",
    ];
    /// Tools a turn that isn't the owner's alone is refused, by name. A tool in
    /// [`CLAUDE_CODE_TOOLS`] must be here or in `NON_OWNER_TOOLS`.
    const REFUSED_TOOLS: &[&str] = &[
        "Task",
        "Agent",
        "CronCreate",
        "CronDelete",
        "CronList",
        "EnterPlanMode",
        "EnterWorktree",
        "ExitWorktree",
        "ListAgents",
        "Monitor",
        "ReportFindings",
        "ScheduleWakeup",
        "SendMessage",
        "Skill",
        "TaskStop",
        "Workflow",
        "RemoteTrigger",
        "TaskOutput",
        "BashOutput",
        "KillShell",
        "PushNotification",
        "SendUserFile",
        "Artifact",
        "mcp__agit__status",
        "mcp__tracker__create_issue",
    ];

    /// Pins the allowlist against Claude Code's real tool set: every tool it offers is either
    /// allowed or refused by name, so a release that adds a tool fails here until it is
    /// placed; each refused one is refused through the hook and each allowed one is left to
    /// Claude Code's own check.
    #[tokio::test]
    async fn every_tool_claude_code_offers_is_placed_by_name() {
        for tool in CLAUDE_CODE_TOOLS {
            assert!(
                NON_OWNER_TOOLS.contains(tool) != REFUSED_TOOLS.contains(tool),
                "{tool}: placed in neither list, or in both"
            );
        }
        for tool in REFUSED_TOOLS {
            assert!(!NON_OWNER_TOOLS.contains(tool), "{tool}");
            assert!(outlives_turn(Some(tool), &json!({})), "{tool}");
        }
        for tool in NON_OWNER_TOOLS {
            assert!(!outlives_turn(Some(tool), &json!({})), "{tool}");
            assert!(
                outlives_turn(Some(tool), &json!({"run_in_background":true})),
                "{tool}"
            );
        }
        held_to_the_owner("RemoteTrigger", json!({"action":"create","body":{}})).await;
        held_to_the_owner("mcp__tracker__create_issue", json!({"title":"later"})).await;
        held_to_the_owner("NewToolFromALaterRelease", json!({})).await;
        let answers =
            hook_answers("Read", json!({"file_path":"a"}), false, &[(false, false)]).await;
        assert_eq!(answers, vec![json!({})]);
    }

    /// Pins that every session agit drives runs with Claude Code's background tasks switched
    /// off, so no running call is moved to the background after a timeout, to deliver a queued
    /// message or on its own, and none can outlive its turn: in the environment, and on the
    /// command line, where no user or project settings file can switch them back on.
    #[test]
    fn a_session_never_moves_a_running_call_to_the_background() {
        let spec = LaunchSpec {
            cwd: PathBuf::from("/tmp"),
            resume_from: None,
            agit_session: None,
            model: None,
            dangerous: false,
            permission_mode: None,
            attach_only: false,
            project_share: false,
        };
        let env = session_env(&spec);
        let args = argv(&spec, "sid", false);
        let pinned = args
            .windows(2)
            .filter(|pair| pair[0] == "--settings")
            .map(|pair| serde_json::from_str::<Value>(&pair[1]).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(pinned.len(), 1, "{args:?}");
        for (key, value) in [
            ("CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", "1"),
            ("CLAUDE_AUTO_BACKGROUND_TASKS", "0"),
        ] {
            assert_eq!(
                env.iter()
                    .filter(|(name, _)| name == key)
                    .map(|(_, set)| set.as_str())
                    .collect::<Vec<_>>(),
                vec![value],
                "{key}"
            );
            assert_eq!(pinned[0]["env"][key], value, "{key} on the command line");
        }
    }

    /// Pins that whose words a turn holds travels with each queued message: a message written
    /// while one turn ran and consumed as the next turn's prompt makes that turn the owner's
    /// only if it, and every message still queued behind it, is the owner's. An echo the driver
    /// can't match is no one's.
    #[tokio::test]
    async fn a_queued_message_decides_the_turn_it_opens() {
        let user = |text: &str| json!({"type":"user","message":{"role":"user","content":text}});
        let result = json!({"type":"result","is_error":false});
        let owns =
            |d: &ClaudeCodeDriver| d.current_turn.is_some() && d.current_turn == d.owner_turn;
        for (queued, expected) in [
            (vec![("next", true)], true),
            (vec![("next", false)], false),
            (vec![("next", true), ("after", false)], false),
            (vec![("next", true), ("after", true)], true),
        ] {
            let mut d = probe();
            d.current_turn = Some("old".into());
            d.owner_turn = Some("old".into());
            for (text, owner) in &queued {
                d.awaiting_echoes.push_back(((*text).into(), *owner));
            }
            assert!(d.classify(result.clone()).is_some(), "the old turn ends");
            assert!(matches!(
                d.classify(user("next")),
                Some(HarnessEvent::TurnStarted { .. })
            ));
            assert_eq!(owns(&d), expected, "{queued:?}");
            if queued.len() > 1 {
                assert!(d.classify(user("after")).is_none());
                assert_eq!(owns(&d), expected, "{queued:?} after the second echo");
            }
            d.shutdown().await.unwrap();
        }
        let mut d = probe();
        assert!(matches!(
            d.classify(user("typed in the terminal")),
            Some(HarnessEvent::TurnStarted { .. })
        ));
        assert!(!owns(&d), "an echo no one wrote is no one's");
        d.shutdown().await.unwrap();
    }

    /// Pins that a turn the device can't attribute fails closed: one Claude Code opened by
    /// itself, a turn the owner opened once a non-owner has written into it, and a call whose
    /// tool can't be read are each refused.
    #[tokio::test]
    async fn a_turn_the_device_cannot_attribute_starts_nothing_that_outlives_it() {
        let input = json!({"cron":"* * * * *","prompt":"later"});
        let unprompted = hook_answers("CronCreate", input.clone(), true, &[]).await;
        assert_eq!(unprompted.len(), 1);
        assert!(refused(&unprompted[0]), "{unprompted:?}");
        let shared = hook_answers(
            "CronCreate",
            input.clone(),
            false,
            &[(false, true), (true, false)],
        )
        .await;
        assert_eq!(shared.len(), 2);
        assert_eq!(shared[0], json!({}), "the owner's own turn");
        assert!(refused(&shared[1]), "after a non-owner's steer: {shared:?}");
        let owners = hook_answers("CronCreate", input, false, &[(false, true), (true, true)]).await;
        assert_eq!(owners, vec![json!({}), json!({})]);
        assert!(outlives_turn(None, &json!({})));
        assert!(outlives_turn(Some("Bash"), &json!("sleep 6 &")));
    }

    /// Pins that the hook is registered in the handshake for every call, and that the rule is
    /// advertised.
    #[test]
    fn the_handshake_registers_the_outlives_turn_hook() {
        assert_eq!(OUTLIVES_TURN_MATCHER, "*", "every call, whatever its tool");
        let advertised = crate::rc::build_identity::DaemonIdentity::current().unwrap();
        assert!(
            advertised
                .rpc_features
                .iter()
                .any(|feature| feature == "nonowner-selfstart-refused-v1"),
            "{:?}",
            advertised.rpc_features
        );
    }

    /// Pins that the rule is advertised only by a build that also holds a non-owner's words to
    /// Default or Plan on every path into a turn, the terminal inbox included: without that, a
    /// non-owner's message into a terminal turn would start work this hook never sees.
    #[test]
    fn the_selfstart_rule_is_advertised_only_with_the_turn_mode_rule() {
        let features = crate::rc::build_identity::DaemonIdentity::current()
            .unwrap()
            .rpc_features;
        if features
            .iter()
            .any(|feature| feature == "nonowner-selfstart-refused-v1")
        {
            assert!(
                features
                    .iter()
                    .any(|feature| feature == "nonowner-turn-mode-v1"),
                "{features:?}"
            );
        }
    }

    /// **A project-scoped command never enters the machine-level catalogue.**
    ///
    /// The catalogue rides along with machine discovery to every workspace on this machine, while
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
    /// and descriptions along with machine discovery to the viewers of every workspace on this
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

    /// Discovery may read the persisted cache before a session handshake. The read path
    /// must filter project commands even when the cache contains them.
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
            attach_only: false,
            project_share: false,
        };
        let args = argv(&spec, "sid", spec.resume_from.is_some());
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

    /// Where Claude refuses to skip permission checks, a new conversation asking for that is
    /// refused before anything starts, and a reopened one continues with approvals; launching
    /// either as asked makes Claude exit, and a reopened one would fail on every retry.
    #[test]
    fn refused_bypass_never_reaches_the_command_line() {
        let spec = |resume_from: Option<&str>| LaunchSpec {
            cwd: PathBuf::from("/tmp"),
            resume_from: resume_from.map(Into::into),
            agit_session: None,
            model: None,
            dangerous: true,
            permission_mode: None,
            attach_only: false,
            project_share: false,
        };
        let mut fresh = spec(None);
        let refused = admit_mode(&mut fresh, true).err().unwrap();
        assert!(!refused.reached_spawn());

        let mut reopened = spec(Some("sid"));
        admit_mode(&mut reopened, true).unwrap();
        assert_eq!(reopened.effective_mode(), PermissionMode::Default);
        assert!(!argv(&reopened, "sid", true).contains(&"--dangerously-skip-permissions".into()));

        let mut allowed = spec(None);
        admit_mode(&mut allowed, false).unwrap();
        assert!(argv(&allowed, "sid", false).contains(&"--dangerously-skip-permissions".into()));
    }

    /// A reopened conversation whose transcript Claude deleted, or never wrote, cannot use
    /// `--resume`: Claude exits at once, and every retry of the same launch fails the same way.
    /// It continues under its own id when nothing was settled, and is refused before the spawn
    /// when its archive already holds turns that a fresh transcript would not continue.
    #[test]
    fn reopening_follows_the_transcript_claude_still_has() {
        let _environment = crate::infra::config::env_lock();
        let root = tempfile::tempdir().unwrap();
        let saved: Vec<_> = ["CLAUDE_CONFIG_DIR", "AGIT_HOME"]
            .into_iter()
            .map(|name| (name, std::env::var_os(name)))
            .collect();
        // SAFETY: this test holds the process-wide configuration lock.
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", root.path().join("claude"));
            std::env::set_var("AGIT_HOME", root.path().join("agit"));
        }
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let id = "90909090-9090-4090-8090-909090909090";
        let mut spec = LaunchSpec {
            cwd: cwd.clone(),
            resume_from: Some(id.into()),
            agit_session: None,
            model: None,
            dangerous: false,
            permission_mode: None,
            attach_only: false,
            project_share: false,
        };
        assert!(
            !reopen(&spec).unwrap(),
            "an unwritten conversation starts under its id"
        );
        assert!(
            argv(&spec, id, false)
                .windows(2)
                .any(|w| w[0] == "--session-id" && w[1] == id)
        );

        let lineage = crate::rc::lineage::AgitSession::new(
            "owner/name",
            "00000000-0000-0000-0000-000000000001",
            "desktop-x",
        )
        .unwrap();
        let repo = lineage.repo_dir().unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args([
                        "-c",
                        "user.name=t",
                        "-c",
                        "user.email=t@t",
                        "-c",
                        "commit.gpgsign=false"
                    ])
                    .args(args)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q", "-b", "desktop-x"]);
        std::fs::write(repo.join(crate::domain::meta::LOG_FILE), "{}\n").unwrap();
        git(&["add", crate::domain::meta::LOG_FILE]);
        git(&["commit", "-q", "-m", "turn"]);
        spec.agit_session = Some(lineage);
        let refused = reopen(&spec).expect_err("archived turns cannot start over");
        assert!(!refused.reached_spawn());
        assert!(refused.to_string().contains("agit resume"));

        let transcript = crate::adapter::claude_code::projects_dir()
            .unwrap()
            .join(crate::adapter::claude_code::slug_for(&cwd))
            .join(format!("{id}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        assert!(reopen(&spec).unwrap(), "a present transcript is resumed");

        for (name, value) in saved {
            // SAFETY: the configuration lock is still held.
            match value {
                Some(value) => unsafe { std::env::set_var(name, value) },
                None => unsafe { std::env::remove_var(name) },
            }
        }
    }

    /// A driver that exists only to feed `classify()`: `cat` stands in for `Proc`, and these
    /// tests never read its output.
    #[tokio::test]
    async fn native_question_answers_preserve_input_and_request_identity() {
        let mut driver = probe();
        let input = json!({"questions":[{"question":"Choose a label", "options":[{"label":"Blue"},{"label":"Green"}], "multiSelect":false}]});
        let event = driver.classify(json!({"type":"control_request","request_id":"native-question", "request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","tool_use_id":"q1","input":input}}));
        assert!(matches!(event, Some(HarnessEvent::Approval(_))));
        let response = ApprovalResponse {
            approval_id: "q1".into(),
            session_id: "test".into(),
            decision: ApprovalDecision::Allow,
            scope: ApprovalScope::Once,
            message: None,
            by: None,
            answers: Some(std::collections::BTreeMap::from([(
                "Choose a label".into(),
                vec!["Blue".into()],
            )])),
        };
        assert!(matches!(
            driver.answer_approval(&response).await,
            ApprovalOutcome::Applied { .. }
        ));
        let line = driver.proc.next().await.unwrap().into_line();
        let Line::Json(value) = line else {
            panic!("expected native response")
        };
        assert_eq!(value["response"]["request_id"], "native-question");
        assert_eq!(
            value["response"]["response"]["updatedInput"]["questions"],
            input["questions"]
        );
        assert_eq!(
            value["response"]["response"]["updatedInput"]["answers"]["Choose a label"],
            "Blue"
        );
        driver.shutdown().await.unwrap();
    }

    fn probe() -> ClaudeCodeDriver {
        ClaudeCodeDriver {
            proc: Proc::spawn("cat", &[], &PathBuf::from("/"), &[]).unwrap(),
            session_id: "sid".into(),
            cwd: PathBuf::from("/"),
            current_turn: None,
            interrupt_draining: false,
            stream_items: Default::default(),
            pending_approvals: Default::default(),
            model: None,
            effort: None,
            effort_known: false,
            model_choice: None,
            model_catalog: vec![],
            pending_model_request: None,
            mode: PermissionMode::Default,
            ready_sent: true,
            commands: vec![],
            pushback: Default::default(),
            awaiting_echoes: Default::default(),
            next_prompt_id: None,
            goal_query_pending: false,
            owner_turn: None,
            next_input_owner: None,
        }
    }

    #[tokio::test]
    async fn replayed_model_control_output_does_not_open_a_phantom_turn() {
        let mut driver = probe();
        let text = "<local-command-stdout>Set model to example</local-command-stdout>";
        let record =
            json!({"type":"user","isReplay":true,"message":{"role":"user","content":text}});
        assert!(driver.classify(record.clone()).is_none());
        assert!(driver.current_turn.is_none());
        driver.awaiting_echoes.push_back((text.into(), false));
        assert!(matches!(
            driver.classify(record),
            Some(HarnessEvent::TurnStarted { .. })
        ));
        driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn model_control_waits_for_native_ack_and_preserves_interleaved_events() {
        let mut driver = probe();
        driver.proc.shutdown().await.unwrap();
        let script = r#"import sys,json
for line in sys.stdin:
    v=json.loads(line)
    assert v['request']['subtype'] in ['set_model', 'apply_flag_settings']
    if v['request']['subtype']=='apply_flag_settings': assert v['request']['settings']['effortLevel'] in ['high',None]
    print(json.dumps({'type':'system','subtype':'status','status':'compacting'}),flush=True)
    print(json.dumps({'type':'control_response','response':{'request_id':v['request_id'],'subtype':'error' if v['request'].get('model')=='refuse' else 'success','response':{}}}),flush=True)
"#;
        driver.proc = Proc::spawn(
            "python3",
            &["-u".into(), "-c".into(), script.into()],
            &PathBuf::from("/"),
            &[],
        )
        .unwrap();
        driver.model_catalog = vec![json!({"id":"native-alias","efforts":[{"id":"high"}]})];
        let response = driver
            .model_control(Some(
                &super::super::models::ModelPatch::parse(&json!({"model":"native-alias"})).unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(response["model"], "native-alias");
        assert_eq!(response["applied"], "immediate");
        assert_eq!(response["effort_known"], true);
        assert!(response["effort"].is_null());
        assert!(
            matches!(driver.pushback.pop_front(), Some(Line::Json(value)) if value["status"] == "compacting")
        );
        assert!(
            driver
                .model_control(Some(
                    &super::super::models::ModelPatch::parse(&json!({"model":"refuse"})).unwrap()
                ))
                .await
                .is_err()
        );
        assert_eq!(
            driver.model_control(None).await.unwrap()["model"],
            "native-alias"
        );
        let patch = super::super::models::ModelPatch::parse(&json!({"effort":"high"})).unwrap();
        assert_eq!(
            driver.model_control(Some(&patch)).await.unwrap()["effort"],
            "high"
        );
        let patch = super::super::models::ModelPatch::parse(&json!({"effort":null})).unwrap();
        let state = driver.model_control(Some(&patch)).await.unwrap();
        assert!(state["effort"].is_null());
        assert_eq!(state["effort_known"], true);
        driver.current_turn = Some("active".into());
        assert!(driver.model_control(Some(&patch)).await.is_err());
        driver.shutdown().await.unwrap();
    }

    /// Pins that a model is named only from the session's catalog, for every caller: an id the
    /// catalog doesn't list is refused before anything reaches Claude Code, and one it does list
    /// (by id or by the model it resolves to) is sent as the catalog's own id, never as the
    /// caller's string.
    #[tokio::test]
    async fn a_model_outside_the_catalog_is_refused_before_claude_hears_of_it() {
        let directory = tempfile::tempdir().unwrap();
        let written = directory.path().join("requests");
        let mut driver = probe();
        driver.proc.shutdown().await.unwrap();
        let script = r#"import sys,json
out=open(sys.argv[1],'a')
for line in sys.stdin:
    v=json.loads(line)
    out.write(json.dumps(v['request'])+'\n'); out.flush()
    print(json.dumps({'type':'control_response','response':{'request_id':v['request_id'],'subtype':'success','response':{}}}),flush=True)
"#;
        driver.proc = Proc::spawn(
            "python3",
            &[
                "-u".into(),
                "-c".into(),
                script.into(),
                written.to_string_lossy().into(),
            ],
            &PathBuf::from("/"),
            &[],
        )
        .unwrap();
        driver.model_catalog = vec![json!({
            "id":"opus","native":{"resolvedModel":"claude-opus-test"},"efforts":[]
        })];
        let requests = || {
            std::fs::read_to_string(&written)
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>()
        };
        for outside in ["claude-unlisted", "Opus"] {
            let patch = super::super::models::ModelPatch::parse(&json!({"model":outside})).unwrap();
            assert!(
                driver.model_control(Some(&patch)).await.is_err(),
                "{outside:?}"
            );
        }
        assert_eq!(
            requests(),
            Vec::<Value>::new(),
            "nothing reached Claude Code"
        );
        let patch =
            super::super::models::ModelPatch::parse(&json!({"model":"claude-opus-test"})).unwrap();
        let state = driver.model_control(Some(&patch)).await.unwrap();
        assert_eq!(state["model"], "opus");
        let sent = requests();
        assert_eq!(sent[0], json!({"subtype":"set_model","model":"opus"}));
        let patch = super::super::models::ModelPatch::parse(&json!({"model":null})).unwrap();
        driver.model_control(Some(&patch)).await.unwrap();
        assert_eq!(
            requests().last().unwrap(),
            &json!({"subtype":"set_model","model":null}),
            "a reset still goes through"
        );
        driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn late_model_ack_resolves_unknown_state_without_starting_a_turn() {
        let mut driver = probe();
        driver.model_catalog = vec![json!({"id":"native","efforts":[{"id":"high"}]})];
        driver.model = Some("native".into());
        driver.pending_model_request = Some((
            "pending".into(),
            json!({"subtype":"apply_flag_settings","settings":{"effortLevel":"high"}}),
        ));
        let state = driver.model_control(None).await.unwrap();
        assert_eq!(state["settings_unknown"], true);
        assert_eq!(state["capabilities"]["model"], false);
        assert!(driver.classify(json!({"type":"control_response","response":{"request_id":"another","subtype":"success"}})).is_none());
        assert!(driver.pending_model_request.is_some());
        assert!(driver.classify(json!({"type":"control_response","response":{"request_id":"pending","subtype":"success"}})).is_none());
        let state = driver.model_control(None).await.unwrap();
        assert_eq!(state["effort"], "high");
        assert_eq!(state["settings_unknown"], false);
        assert!(driver.current_turn.is_none());
        driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn api_failure_preserves_native_diagnostics_and_closes_only_its_turn() {
        let mut driver = probe();
        for (payload, expected) in [
            (
                json!({"type":"result", "subtype":"success", "is_error":true,
                "result":"API Error: 403 provider refused this client"}),
                Some("API Error: 403 provider refused this client"),
            ),
            (
                json!({"type":"result", "subtype":"error_during_execution", "is_error":true,
                "errors":["API request failed", "Retry limit reached"], "result":""}),
                Some("API request failed\nRetry limit reached"),
            ),
            (
                json!({"type":"result", "is_error":true, "errors":[null, " ", {}]}),
                Some("Claude Code ended the turn with an error."),
            ),
            (
                json!({"type":"result", "is_error":false, "result":"normal answer"}),
                None,
            ),
        ] {
            assert!(matches!(
                driver.classify(
                    json!({"type":"user", "message":{"role":"user", "content":"next prompt"}})
                ),
                Some(HarnessEvent::TurnStarted { .. })
            ));
            let Some(HarnessEvent::TurnCompleted {
                error,
                outcome,
                turn_id,
                ..
            }) = driver.classify(payload)
            else {
                panic!("a native result must close its open turn");
            };
            assert_eq!(error.as_deref(), expected);
            assert!(!turn_id.is_empty());
            assert_eq!(matches!(outcome, TurnOutcome::Error), expected.is_some());
            assert!(driver.current_turn.is_none());
        }
        driver.shutdown().await.unwrap();
    }

    /// The test driver, writing its stdin into the named pipe `log`, which this creates. The
    /// writer blocks until `written` opens the pipe for reading.
    #[cfg(unix)]
    fn logging_driver(log: &std::path::Path) -> ClaudeCodeDriver {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(log.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0, "mkfifo");
        let mut driver = ClaudeCodeDriver::test_driver();
        driver.proc = Proc::spawn(
            "sh",
            &[
                "-c".into(),
                "cat > \"$1\"".into(),
                "log".into(),
                log.display().to_string(),
            ],
            &PathBuf::from("/"),
            &[],
        )
        .unwrap();
        driver
    }

    /// The lines `driver` wrote before a sentinel written now: the pipe is read in order, with
    /// blocking reads, up to that sentinel.
    #[cfg(unix)]
    async fn written(driver: &mut ClaudeCodeDriver, log: &std::path::Path) -> Vec<Value> {
        use std::io::BufRead;
        driver
            .proc
            .write_line(&json!({"type":"sentinel"}))
            .await
            .unwrap();
        let log = log.to_owned();
        tokio::task::spawn_blocking(move || {
            let reader = std::io::BufReader::new(std::fs::File::open(&log).unwrap());
            let mut lines = Vec::new();
            for line in reader.lines() {
                let line: Value = serde_json::from_str(&line.unwrap()).unwrap();
                if line["type"] == "sentinel" {
                    return lines;
                }
                lines.push(line);
            }
            panic!("the pipe closed before the sentinel");
        })
        .await
        .unwrap()
    }

    #[cfg(unix)]
    fn interrupts(lines: &[Value]) -> usize {
        lines
            .iter()
            .filter(|line| line["request"]["subtype"] == "interrupt")
            .count()
    }

    /// A fenced interrupt is refused whatever turn it names, the running one included, and
    /// nothing reaches Claude Code; an unfenced interrupt stops whatever runs, as before.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_fenced_interrupt_is_refused_and_an_unfenced_one_goes_out() {
        let directory = tempfile::tempdir().unwrap();
        for (case, current) in [
            ("running", Some("t1")),
            ("later", Some("t2")),
            ("idle", None),
        ] {
            let log = directory.path().join(case);
            let mut driver = logging_driver(&log);
            match current {
                Some(turn) => driver.set_test_current_turn(turn),
                None => driver.clear_test_current_turn(),
            }
            let error = driver.interrupt_fenced(Some("t1")).await.unwrap_err();
            assert!(error.is::<super::super::FenceUnsupported>(), "{case}");
            assert_eq!(interrupts(&written(&mut driver, &log).await), 0, "{case}");
            assert_eq!(driver.current_turn(), current, "{case}");
            driver.shutdown().await.unwrap();
        }

        let log = directory.path().join("unfenced");
        let mut driver = logging_driver(&log);
        driver.set_test_current_turn("t2");
        assert!(matches!(
            driver.interrupt_fenced(None).await,
            Ok(super::super::InterruptOutcome::Requested)
        ));
        assert_eq!(interrupts(&written(&mut driver, &log).await), 1);
        driver.shutdown().await.unwrap();
    }

    /// The Hub reads `interrupt_fenced == true` on a Claude Code session as a terminal-attached
    /// conversation, so a headless session keeps reporting false.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_headless_session_still_reports_no_interrupt_fence() {
        let mut driver =
            super::super::AnyDriver::ClaudeCode(Box::new(ClaudeCodeDriver::test_driver()));
        assert!(!driver.interrupt_fenced());
        driver.shutdown().await.unwrap();
    }

    fn interrupted_line() -> Value {
        json!({"type":"user","message":{"role":"user",
               "content":[{"type":"text","text":"[Request interrupted by user]"}]}})
    }

    #[tokio::test]
    async fn an_interrupt_result_must_drain_before_a_new_prompt_is_admitted() {
        let mut driver = probe();
        driver.current_turn = Some("interrupted-turn".into());
        assert!(driver.classify(interrupted_line()).is_some());
        assert!(matches!(
            driver.start_turn("next prompt").await,
            super::super::TurnStartOutcome::RetryableNotAccepted { .. }
        ));
        assert!(driver.current_turn.is_none());
        assert!(
            driver
                .classify(json!({"type":"result","is_error":true}))
                .is_none()
        );
        assert!(matches!(
            driver.start_turn("next prompt").await,
            super::super::TurnStartOutcome::Accepted { .. }
        ));
        let current = driver.current_turn.clone().unwrap();
        assert!(
            matches!(driver.classify(json!({"type":"result","is_error":false})), Some(HarnessEvent::TurnCompleted { turn_id, .. }) if turn_id == current)
        );
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
            answers: None,
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
            answers: None,
            by: Some("owner".into()),
        }
    }

    fn seed_session_approval(d: &mut ClaudeCodeDriver) {
        d.pending_approvals.insert(
            "approval-1".into(),
            PendingApproval {
                request_id: "native-request-1".into(),
                question_input: None,
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

    #[tokio::test]
    async fn remote_prompt_identity_reaches_native_input_for_start_and_steer() {
        for start in [true, false] {
            let mut driver = probe();
            driver.current_turn = if start { None } else { Some("turn".into()) };
            let mut driver = crate::rc::harness::AnyDriver::ClaudeCode(Box::new(driver));
            let identity = driver.reserve_prompt_identity().expect("native identity");
            assert!(uuid::Uuid::parse_str(&identity).is_ok());
            if start {
                assert!(matches!(
                    driver.start_turn("continue", false, None).await,
                    crate::rc::harness::TurnStartDispatch::Resolved(
                        crate::rc::harness::TurnStartOutcome::Accepted { .. }
                    )
                ));
            } else {
                assert!(matches!(
                    driver.steer("continue").await,
                    crate::rc::harness::SteerDispatch::Resolved(Ok(_))
                ));
            }
            let crate::rc::harness::AnyDriver::ClaudeCode(ref mut native) = driver else {
                unreachable!()
            };
            let line = tokio::time::timeout(std::time::Duration::from_secs(1), native.proc.next())
                .await
                .unwrap()
                .unwrap()
                .into_line();
            let Line::Json(record) = line else {
                panic!("expected native prompt")
            };
            assert_eq!(record["uuid"], identity);
            assert_eq!(record["message"]["content"], "continue");
            assert!(native.next_prompt_id.is_none());
            driver.shutdown().await.unwrap();
        }
    }

    /// `--replay-user-messages` is required (other viewers must see what the
    /// driver typed) but its echo must not open a second turn.
    #[tokio::test]
    async fn the_replayed_echo_of_our_own_message_does_not_open_a_second_turn() {
        let mut d = probe();
        d.current_turn = Some("turn-1".into());
        d.awaiting_echoes.push_back(("hello".into(), false));
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
        assert!(error.is::<super::super::TurnNotRunning>());
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
                .map(|(echo, _)| echo.as_str())
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
            attach_only: false,
            project_share: false,
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
