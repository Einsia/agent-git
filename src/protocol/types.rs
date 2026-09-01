//! Typed params / results / event payloads for every RC method.
//!
//! Naming follows the JSON: snake_case fields, snake_case enum variants. Every
//! struct that can grow gets `#[serde(default)]` on optional fields so an older
//! peer keeps parsing frames from a newer one (forward compatibility is cheaper
//! than a version negotiation dance for a protocol this young).
//!
//! The IR types carried on the wire are re-exported from `crate::adapter` — not
//! redefined — so the web renders exactly what `agit show` renders.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub use crate::adapter::{Event as IrEvent, EventKind as IrEventKind};

// ─────────────────────────────────────────────────────────────────────────────
// Capabilities
// ─────────────────────────────────────────────────────────────────────────────

/// When a `turn.steer` message actually reaches the model.
///
/// Measured, not assumed (PRD §3.5): claude-code's stream-json channel accepts a
/// second user message immediately but only *delivers* it when the in-flight
/// tool call returns (31 s in the test). codex's `turn/steer` is a protocol verb
/// and lands at once. The difference is user-visible, so it is on the wire and
/// the UI must show "queued — delivered after the current tool finishes".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Immediate,
    AtToolBoundary,
}

/// How much the agent may do without asking.
///
/// # Why a harness-neutral vocabulary rather than passing the native value
///
/// The two harnesses do not agree on the shape of this concept, let alone the
/// values. claude-code has one scalar (`--permission-mode`: `acceptEdits`,
/// `auto`, `bypassPermissions`, `plan`, …). codex has **two orthogonal axes**
/// (`approvalPolicy` × `sandbox`). Putting either one's native form on the wire
/// would make the web page speak one harness's dialect and mistranslate the
/// other — and the whole point of this protocol is that a viewer drives a
/// session without knowing which harness is underneath.
///
/// So the wire carries intent, each driver translates, and
/// [`RuntimeCapability::permission_modes`] says which of these a given runtime
/// can actually express. A mode a runtime cannot honour is never offered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Ask before anything the machine's own configuration does not already
    /// allow. The safe default, and what a fresh terminal session does.
    Default,
    /// File edits land without asking; commands still stop for approval.
    AcceptEdits,
    /// "Auto" — everything the sandbox permits proceeds unattended. Still
    /// bounded by the workspace allowlist, which lives on the machine.
    Auto,
    /// Look but do not touch: no edits, no commands. For planning.
    Plan,
    /// No permission checks at all. Owner-only, and the session is marked
    /// [`SessionInfo::dangerous`] for the rest of its life.
    Bypass,
}

impl PermissionMode {
    /// Does this mode hand over an unchecked machine?
    ///
    /// Used in two places that must agree: the hub restricts the switch to the
    /// owner, and the daemon marks the session dangerous. Keeping it one
    /// function is what keeps them from drifting apart.
    pub fn is_dangerous(self) -> bool {
        matches!(self, PermissionMode::Bypass)
    }

    /// Does this mode **loosen** the guard?
    ///
    /// Separate from [`PermissionMode::is_dangerous`] because the two answer
    /// different questions. `is_dangerous` answers "has this session ever been
    /// handed over" — only `bypass` does, and it drives that monotonic bit.
    /// `loosens_guard` answers "does this switch need the owner", and that
    /// answer runs one notch wider: under `accept_edits` claude-code **stops
    /// sending `can_use_tool` at all**, so the approval classifier is never
    /// called once; `auto` likewise. Loosening is not narrowed to "equals
    /// `bypass`", because the test "anything that is not `bypass` passes" lets
    /// an operator switch the whole classifier off in a single frame.
    ///
    /// The direction is deliberate: a mode added later lands on the
    /// **loosening** side by default (owner required), instead of falling
    /// silently to the operator because its string is not `bypass`.
    pub fn loosens_guard(self) -> bool {
        matches!(
            self,
            PermissionMode::AcceptEdits | PermissionMode::Auto | PermissionMode::Bypass
        )
    }

    /// How strict. Lower is stricter.
    ///
    /// # Why the target mode alone is not enough
    ///
    /// `loosens_guard` asks "is this mode itself loose", and one mode is
    /// **stricter** than `default`: the whole point of `plan` is "look but do
    /// not touch". So `plan → default` is a real loosening (from "may not
    /// touch" to "may touch after asking") that reads as a tightening because
    /// `Default.loosens_guard()` is false — one operator then releases a
    /// session the owner deliberately confined to `plan`, and `resume_session`
    /// carries that mode across restarts precisely so this cannot happen.
    ///
    /// Loosening is judged by **where it comes from and where it goes**, which
    /// needs an order.
    pub fn strictness(self) -> u8 {
        match self {
            // Look but do not touch.
            PermissionMode::Plan => 0,
            // Ask about everything.
            PermissionMode::Default => 1,
            // File edits stop asking; commands still ask.
            PermissionMode::AcceptEdits => 2,
            // Everything the sandbox permits is allowed.
            PermissionMode::Auto => 3,
            // No checks.
            PermissionMode::Bypass => 4,
        }
    }

    /// Is the switch from `from` to `self` a **loosening**?
    ///
    /// With the starting point known there is one test: **compare them**.
    /// `plan → default` is a loosening (from "look but do not touch" to "may
    /// touch after asking"); `bypass → auto` is a tightening — even though
    /// `auto` is a loose mode on its own.
    ///
    /// Only an unknown starting point falls back to the absolute test
    /// (`loosens_guard`); see the `from: None` arm of
    /// `require_owner_to_loosen`. Or-ing the two together makes every target in
    /// `accept_edits` / `auto` / `bypass` count as a loosening, so
    /// `bypass → auto` — an outright **tightening** — would need the owner to
    /// nod as well, and with the owner away a runaway session stays loose,
    /// which is the one thing this test exists to prevent.
    pub fn loosens_from(self, from: PermissionMode) -> bool {
        self.strictness() > from.strictness()
    }
}

/// When a permission-mode change takes effect.
///
/// Not cosmetic: on codex the policy rides along with `turn/start` as a sticky
/// override, so a switch made while a turn is running cannot apply until the
/// next one. The UI has to say so — silently accepting a change that will not
/// happen for minutes is how a user ends up believing the machine ignored them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionApply {
    /// Live, mid-turn included.
    Immediate,
    /// Stored; applied when the next turn starts.
    NextTurn,
}

/// What one harness on one machine can do under RC. Reported in `rc.register`;
/// the frontend renders controls from it (no interrupt button for a harness
/// that cannot interrupt).
///
/// Deliberately separate from `adapter::Capability` (ImportOnly / ExportOnly /
/// Resumable): that enum answers "can a *transcript* be installed", this one
/// answers "can a *live process* be driven". They vary independently.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeCapability {
    /// Normalized runtime id: `claude-code` | `codex` | …
    pub runtime: String,
    /// Executable found on PATH.
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// `None` = no mid-turn steer at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steer: Option<Delivery>,
    #[serde(default)]
    pub interrupt: bool,
    /// Approval requests can be routed to a remote viewer.
    #[serde(default)]
    pub approvals: bool,
    /// Streams token-level deltas.
    #[serde(default)]
    pub partial_messages: bool,
    /// Can resume an existing session by id.
    #[serde(default)]
    pub resume: bool,
    /// Slash commands this runtime accepts, harvested from its handshake.
    ///
    /// This is what makes the web feel like the terminal rather than a chat box
    /// bolted onto one. Both harnesses accept a command as an ordinary message
    /// whose text begins with `/`; they execute it locally and (for commands
    /// like `/context` or `/goal`) answer without a model round trip at all.
    /// Measured on claude-code 2.1.233: the `initialize` response lists 48
    /// commands including `/goal` and `/compact`, and `/goal` alone replies
    /// `No goal set. Usage: /goal <condition>` with `num_turns = 0`.
    #[serde(default)]
    pub commands: Vec<SlashCommand>,
    /// Permission modes this runtime can actually express, in the order a UI
    /// should offer them (loosest guard first). Empty = the runtime has no
    /// notion of permission modes and the control should not be rendered.
    #[serde(default)]
    pub permission_modes: Vec<PermissionMode>,
    /// When a switch takes effect, or `None` if it cannot be switched after
    /// launch. See [`PermissionApply`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_switch: Option<PermissionApply>,
}

/// One slash command offered by a runtime. Rendered as an autocomplete entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashCommand {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// e.g. `<condition>` for `/goal`. Shown as ghost text in the composer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// rc.* — agitd ↔ hub
// ─────────────────────────────────────────────────────────────────────────────

/// A workspace as `agitd` knows it locally. Reported at register so the hub can
/// reconcile both ways (hub-has/machine-lacks → stale; machine-has/hub-lacks →
/// offer import). Neither side ever deletes the other's record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalWorkspace {
    pub workspace_id: String,
    #[serde(default)]
    pub projects: Vec<LocalProject>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalProject {
    pub project_id: String,
    /// Absolute path on the machine.
    pub local_path: String,
    /// Directory exists right now (it may have been deleted since binding).
    pub exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_origin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RcRegister {
    pub protocol_version: u32,
    /// Generated once at first pairing, stored under `~/.agit/rc/identity`.
    /// Stable across reconnects and reboots; changes only on reinstall. The hub
    /// upserts on `(account_id, machine_fingerprint)` so reconnecting never
    /// consumes a second quota slot.
    pub machine_fingerprint: String,
    pub display_name: String,
    pub agit_version: String,
    /// `linux-x86_64`, `macos-aarch64`, …
    pub platform: String,
    pub capabilities: Vec<RuntimeCapability>,
    /// Additive protocol features this daemon understands. The hub must echo a
    /// feature in [`RcRegisterResult::accepted_features`] before either side
    /// uses it for security-sensitive wire semantics.
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub workspaces: Vec<LocalWorkspace>,
    /// Highest seq `agitd` has emitted per stream. Lets the hub detect a gap
    /// or a regression on the very first frame after reconnect.
    #[serde(default)]
    pub last_seq: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RcRegisterResult {
    pub connection_id: String,
    /// Intersection of the daemon's advertised features and the hub's
    /// supported features. Missing means an old hub and therefore ACKs
    /// nothing.
    #[serde(default)]
    pub accepted_features: Vec<String>,
    /// Hub's view of the workspaces bound to this connection. `agitd` reconciles
    /// its local mirror against this.
    #[serde(default)]
    pub workspaces: Vec<HubWorkspace>,
    /// Highest seq the hub has durably persisted per stream.
    ///
    /// **This raises agitd's local watermark; it does not trigger a replay.**
    /// Reconnecting is not a replay: agitd resumes numbering above whatever the
    /// hub already has so a restarted daemon never re-issues a seq that is
    /// already durable. Backfilling history is a *viewer*-driven act — it
    /// happens only when someone sends `session.subscribe(after_seq)`.
    ///
    /// The earlier wording ("agitd replays anything above this") described a
    /// behaviour that has never existed. A client written against it would sit
    /// waiting for frames nobody is going to send.
    #[serde(default)]
    pub persisted_seq: BTreeMap<String, u64>,
    pub server_time: String,
}

/// A workspace as the hub knows it (definition state, Aurora truth).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HubWorkspace {
    pub workspace_id: String,
    pub name: String,
    #[serde(default)]
    pub projects: Vec<HubProject>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HubProject {
    pub project_id: String,
    pub local_path: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// workspace.* / project.* / fs.* — viewer → agitd (relayed by hub)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WorkspaceList {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceListResult {
    pub workspaces: Vec<LocalWorkspace>,
}

/// Bind a local directory into a workspace. The hub creates the row *after*
/// `agitd` confirms the path exists and is a directory — the machine is the
/// authority on its own filesystem.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectBind {
    pub workspace_id: String,
    pub project_id: String,
    pub local_path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectBindResult {
    pub project_id: String,
    /// Canonicalized absolute path (symlinks resolved) — this is what the
    /// allowlist stores and compares against.
    pub local_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_origin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectUnbind {
    pub workspace_id: String,
    pub project_id: String,
}

/// List a directory. Used by the folder picker before any project exists, so it
/// is scoped by *ownership* not by allowlist: only the connection's owner may
/// call it, only under `$HOME`, and it returns names — never file contents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsReadDirectory {
    /// Absolute path, or empty for the home directory.
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsReadDirectoryResult {
    pub path: String,
    pub entries: Vec<DirEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
    /// Directory contains a `.git` (a likely project root).
    #[serde(default)]
    pub is_git_repo: bool,
}

/// Read a file for the preview pane in the top right.
///
/// It goes through agitd rather than letting the browser reach the machine
/// directly: the allowlist lives on the machine, and reading a file passes the
/// same gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsReadFile {
    pub workspace_id: String,
    /// Absolute path; must fall inside a project bound to this workspace.
    pub path: String,
    /// Byte offset to read from (a large file is chunked).
    #[serde(default)]
    pub offset: u64,
}

/// The content of one file preview.
///
/// Text goes as a string; binary (image / PDF / audio) as base64 — the preview
/// pane wants something it can drop straight into `<img src>` / `<embed>` /
/// `<audio>`, and a separate binary channel for each is not worth it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FsReadFileResult {
    pub path: String,
    pub size: u64,
    /// Guessed MIME; the frontend picks the preview pane from it.
    pub mime: String,
    /// Text content (present when `is_binary == false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// base64 (present when `is_binary == true`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base64: Option<String>,
    pub is_binary: bool,
    /// The file holds more content past this nominal byte window.
    #[serde(default)]
    pub truncated: bool,
}

/// Nominal window cap for one file preview (bytes).
///
/// Over the cap it truncates instead of refusing: the head of a 200 MB log is
/// still worth reading, while pushing the whole thing through the WebSocket
/// wedges the page and the bus together. A text response may carry up to three
/// extra continuation bytes to complete the UTF-8 character at the end of the
/// window; that character belongs to this window by its leading byte, and the
/// next chunk's fixed offset skips the leading continuation bytes, so the
/// pieces join with nothing repeated and nothing lost.
pub const FILE_PREVIEW_CAP: u64 = 2 * 1024 * 1024;

// ─────────────────────────────────────────────────────────────────────────────
// terminal.* — the terminal in the bottom right
//
// This is the one place that genuinely needs a PTY, and for a reason the
// sessions do not share: **a human is on this end**. A session wants structure
// (which span is a tool call, an approval rendered as two buttons); a terminal
// wants exactly that rendered byte stream — vim, top, colored build output all
// live on it. So the terminal runs over a PTY and the session over the
// structured protocol, and neither disturbs the other.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalOpen {
    pub workspace_id: String,
    /// Which project directory to open in. Omitted = the workspace's first
    /// project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
}

fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalOpenResult {
    pub terminal_id: String,
    pub cwd: String,
    pub shell: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalInput {
    pub terminal_id: String,
    /// Raw bytes the user typed (control characters included).
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalResize {
    pub terminal_id: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalClose {
    pub terminal_id: String,
}

/// Bytes the terminal emitted.
///
/// **It carries a seq, and nothing is backfilled.** The number is there for the
/// hub (`Frame::is_event()` needs it, and so does `(stream, seq)`
/// deduplication), while the bytes themselves enter no replay buffer — a PTY is
/// a stream, not a record. Reconnecting does not recover the output from the
/// disconnected stretch; when the link stalls and bytes are dropped, the
/// machine writes a visible mark into that terminal. See the exception in the
/// `crate::protocol` module docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalOutput {
    pub terminal_id: String,
    pub data: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TerminalExited {
    pub terminal_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
}

// ─────────────────────────────────────────────────────────────────────────────
// session.* — viewer → agitd
// ─────────────────────────────────────────────────────────────────────────────

/// Start a new session. Maps to `agit new`: mints a branch on the agent repo,
/// launches the harness headless under `agitd`'s supervision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStart {
    /// Stable UUID for this launch intent. Required only after both peers have
    /// negotiated `session_start_idempotency_v1`; omitted by legacy hubs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_id: Option<String>,
    pub workspace_id: String,
    pub project_id: String,
    /// `claude-code` | `codex`
    pub runtime: String,
    /// `owner/name` of the agent repo to record into. Optional: without it the
    /// session runs but is not under version control until `agit import`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Immutable identity of `agent`. Meaningful only after both peers have
    /// negotiated `agent_identity_v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// First message. Optional so a viewer can open an idle session and type
    /// into it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Who asked (account id). For the audit trail and `turn.started.by`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Permission mode to launch under. Omitted = [`PermissionMode::Default`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStartResult {
    /// Echo of the negotiated idempotency key. Legacy starts omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_id: Option<String>,
    pub session: SessionInfo,
}

/// Resume an existing branch. Maps to `agit resume` including its fast path
/// (native resume, zero-copy) and slow path (materialize + mint a new runtime
/// id). Which path was taken is invisible on the wire — that is the point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionResume {
    pub workspace_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Where this conversation should settle, filled in by the hub.
    ///
    /// Only used when taking over a session the machine has no record of — one
    /// started in a terminal. A session the daemon already knows keeps the
    /// lineage in its roster, and that one wins: re-pointing an existing
    /// conversation at a different branch would split its history in two.
    ///
    /// The hub was already injecting these two into the resume params; the
    /// daemon just had nowhere to put them, so every taken-over terminal
    /// session settled nowhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Immutable identity of `agent`. Meaningful only after both peers have
    /// negotiated `agent_identity_v1`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionResumeResult {
    pub session: SessionInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionList {
    pub workspace_id: String,
    /// Also list the local sessions on the machine **this workspace has not
    /// taken over yet**.
    ///
    /// This is the entry point for "take over work already running on the
    /// laptop": the user has been talking in a terminal for a while and wants
    /// to carry on from the web instead of opening an empty session.
    #[serde(default)]
    pub include_local: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionListResult {
    /// The **live** sessions in this workspace (agitd is supervising them).
    pub sessions: Vec<SessionInfo>,
    /// Local sessions on the machine that can be taken over.
    #[serde(default)]
    pub local: Vec<LocalSession>,
}

/// A session that already exists on the machine and has not been taken over by
/// RC.
///
/// Carries only the fields reachable **without opening the transcript** — the
/// cost of listing must not grow with the number of sessions on disk (observed
/// on this machine: 18858 sessions, 390 ms to parse them all). `gist` is the
/// exception, see [`LOCAL_GIST_BUDGET`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalSession {
    /// The harness's own session id (claude's uuid / codex's thread id).
    pub runtime_session_id: String,
    pub runtime: String,
    pub cwd: String,
    /// Last modification time of the transcript file (RFC3339), for sorting by
    /// "talked to most recently".
    pub modified_at: String,
    /// Gist of the opening prompt. Computed only for the most recent sessions,
    /// `None` for the rest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gist: Option<String>,
    /// Already adopted by agit (the store holds a link). Taking it over
    /// continues that lineage.
    #[serde(default)]
    pub adopted: bool,
    /// The agent it is managed by (`owner/name`), if it was ever adopted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The transcript file was still growing a moment ago — **this session is
    /// probably open in someone else's terminal**.
    ///
    /// Taking over a live session is not "continuing it", it is **opening a
    /// second writer on the same transcript file**: the original process keeps
    /// appending, the process started by `--resume` appends too, and once the
    /// two streams of writes interleave both histories are destroyed. So this
    /// is not a hint, it is a stop line — the UI must block, or at least send
    /// the user to that terminal to exit first.
    ///
    /// The test is only mtime, so it is **conservative**: "live" may mean it
    /// only just ended, while "not live" holds. Better one block too many.
    #[serde(default)]
    pub likely_active: bool,
}

/// A transcript file touched within this window counts as still open (seconds).
///
/// 90 seconds: inside one long turn the model may write nothing for tens of
/// seconds (running tests, waiting on a compile), and a window narrower than
/// that calls a thinking session idle, which is the worst direction to be wrong
/// in.
pub const LIKELY_ACTIVE_SECS: i64 = 90;

/// How many local sessions at most get a `gist`.
///
/// Computing one gist parses a whole transcript (about 6.8 ms for a 3 MB
/// session). Computing it for the most recent 12 keeps the cost a constant,
/// the same whether the disk holds 18858 sessions or 3 — the shape the CLI rule
/// "an operation that grows with the session count must not open a transcript"
/// allows.
pub const LOCAL_GIST_BUDGET: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Running,
    AwaitingApproval,
    /// The connection is offline; the session may still be running on the
    /// machine, but nobody can observe or steer it until `agitd` reconnects.
    Detached,
    /// The harness process exited.
    Ended,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    /// Logical session id (`agit-…`) = branch. Never the harness's thread id.
    pub session_id: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub runtime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub status: SessionStatus,
    /// Highest seq emitted so far.
    pub last_seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gist: Option<String>,
    /// Started with permission checks disabled (`--dangerously-skip-permissions`,
    /// codex `dangerFullAccess`). Sharing such a session is handing out an
    /// unsupervised root shell; the hub locks it to the owner.
    #[serde(default)]
    pub dangerous: bool,
    /// How much this session may do without asking, right now. Carried on the
    /// session rather than inferred from the last event, so a viewer that joins
    /// late still renders the control correctly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
    pub created_at: String,
    pub updated_at: String,
}

/// Subscribe to a session's event stream. `after_seq` is the last seq the
/// client has; the server replays everything above it (from the hub's Redis
/// window, or on a miss, from `agitd`'s ring buffer).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSubscribe {
    pub session_id: String,
    #[serde(default)]
    pub after_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSubscribeResult {
    pub session: SessionInfo,
    /// Events with seq in `(after_seq, from_seq)` are gone (older than every
    /// buffer). The client should show a "history truncated" marker; the full
    /// transcript remains available via the committed session.
    #[serde(default)]
    pub from_seq: u64,
}

/// Follow a session that is live in someone's terminal, read-only.
///
/// Watching starts no process and writes nothing — it tails the transcript the
/// harness is already appending to. That is why it is safe on a session that
/// would be refused by `session.resume` (a second writer corrupts both
/// histories; a reader corrupts nothing).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionWatch {
    pub workspace_id: String,
    /// The harness-native session id (from the local-sessions list).
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionWatchResult {
    pub session: SessionInfo,
    /// The first transcript line the live stream will replay from.
    pub from_line: u64,
    /// Total lines in the transcript at watch time.
    pub total_lines: u64,
    /// Whether the two numbers above **are** physical line numbers in the
    /// transcript.
    ///
    /// Usually they are. But an absurdly large transcript is not scanned whole
    /// — that scan runs synchronously under the daemon's global lock, its cost
    /// grows linearly with file size, and a big enough file stalls every RPC on
    /// this machine, event pump included, for hundreds of milliseconds. Past
    /// the `tail_window` scan limit only the last stretch is scanned, so the
    /// line numbers are **relative to the truncation point**.
    ///
    /// **This has to be said, not silently swapped.** Line numbers elsewhere in
    /// the protocol (`agit show`, `item.completed`'s `line`) are physical ones;
    /// a number quietly turned relative points the two sides at different
    /// records, with no symptom at all. When false, a caller may treat these
    /// numbers only as "ordinals inside this stream" and must not align them
    /// with committed history.
    #[serde(default = "yes")]
    pub absolute_lines: bool,
    /// Always true — the viewer must disable its composer.
    pub read_only: bool,
}

/// An older daemon sends no `absolute_lines`, and its numbers **are** physical
/// — the default must be true, or upgrading the hub makes every watch on an
/// older machine look truncated.
fn yes() -> bool {
    true
}

/// `session.setPermissionMode` — change how much the agent may do unattended.
///
/// This is the web's equivalent of the terminal's mode switcher: driving a
/// session from a browser is still driving *that* session, and a driver who
/// cannot loosen or tighten the guard has to fall back to a terminal for the
/// one decision they most often need to make.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSetPermissionMode {
    pub session_id: String,
    pub mode: PermissionMode,
    /// Who asked (account id), for the audit trail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSetPermissionModeResult {
    pub mode: PermissionMode,
    /// Whether it is live already or waits for the next turn.
    pub applied: PermissionApply,
}

/// `session.permissionMode` — broadcast after a successful change.
///
/// Sent to every viewer, not just the one who asked: two people watching one
/// session must not disagree about how much it is allowed to do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionPermissionMode {
    pub session_id: String,
    pub mode: PermissionMode,
    pub applied: PermissionApply,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// turn.* — viewer → agitd
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnStart {
    pub session_id: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Client-generated id for idempotency (weak network → resend). The hub
    /// dedupes on it for 10 minutes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_msg_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnStartResult {
    pub turn_id: String,
}

/// Add to the *current* turn without stopping it. Distinct verb from interrupt
/// on purpose — "also do X" and "stop, don't do that" are different intents and
/// the UI must not make the user express them by wording.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnSteer {
    pub session_id: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_msg_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnSteerResult {
    /// The UI shows a "queued" state when this is `AtToolBoundary`.
    pub delivery: Delivery,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnInterrupt {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TurnInterruptResult {}

// ─────────────────────────────────────────────────────────────────────────────
// Events — agitd → viewers (notifications with seq + stream)
// ─────────────────────────────────────────────────────────────────────────────

/// Coarse item kinds for streaming. The precise kind lands with
/// `item.completed` as an [`IrEvent`]; this is only so a viewer can render a
/// placeholder while deltas arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    UserMessage,
    AssistantMessage,
    Reasoning,
    ToolCall,
    ToolResult,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemStarted {
    pub item_id: String,
    pub turn_id: String,
    pub kind: ItemKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemDelta {
    pub item_id: String,
    pub text: String,
}

/// A completed item. This is the unit the hub persists and the frontend
/// renders permanently. Two representations of the same thing:
///
/// * `event` — the IR (`adapter::Event`), identical to what `agit show` shows.
/// * `raw` + `object_hash` — the harness's own transcript line and its
///   canonical hash (`domain::transcript::object_hash`). This is *the same
///   hash* the committed `transcript.jsonl` envelope carries, so the hub can
///   reconcile its live projection against the pushed history line by line.
///
/// `agitd` produces these by tailing the harness's transcript file, not by
/// re-deriving them from stdout: the file is what `agit commit` will snapshot,
/// so the live stream and the settled history cannot disagree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemCompleted {
    pub item_id: String,
    pub turn_id: String,
    pub event: IrEvent,
    /// 0-based line in the transcript file (`event.line` mirrors it).
    pub line: u64,
    pub object_hash: String,
    /// The raw transcript line. Capped at [`RAW_LINE_CAP`] bytes on the live
    /// path; the committed transcript is never truncated.
    pub raw: Value,
    #[serde(default)]
    pub raw_truncated: bool,
}

/// Cap for `ItemCompleted::raw` on the live path (bytes). Tool results can be
/// megabytes; streaming them to every viewer buys nothing the committed
/// transcript doesn't already hold.
pub const RAW_LINE_CAP: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnSource {
    /// Typed into the local terminal / harness UI.
    Local,
    /// Sent through RC by an account.
    Remote,
    /// Started by the harness itself (e.g. hook-driven).
    System,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnStarted {
    pub turn_id: String,
    pub source: TurnSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// The user text that opened the turn (for the timeline header).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Ok,
    Interrupted,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnCompleted {
    pub turn_id: String,
    pub outcome: TurnOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Harness-reported cost, when available (claude-code `result` frame).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatusChanged {
    pub session_id: String,
    pub status: SessionStatus,
}

/// A device-local registered secret was removed before an event left the machine.
/// Deliberately generic: the hub must not become an oracle for individual local rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretDetected {
    /// Newly seen registered rules in this session. Repeated occurrences are deduplicated.
    pub count: usize,
    /// Coarse event class (`turn_prompt`, `item_delta`, `item_completed`, `approval`).
    pub source: String,
}

/// A turn was settled into the agent repo (`agit commit`). Lets the hub verify
/// its projection against the commit and, after `agit push`, against the pushed
/// transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommitSettled {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Immutable identity of `agent`. A new daemon emits this notification only
    /// while `agent_identity_v1` remains ACKed for the current connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub commit_sha: String,
    /// Highest seq covered by this commit.
    pub through_seq: u64,
    /// Number of turns in the chain after this commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turns: Option<u32>,
}

// ─────────────────────────────────────────────────────────────────────────────
// approval.* — agitd → viewer (request), viewer → agitd (response)
// ─────────────────────────────────────────────────────────────────────────────

/// Three classes because they route differently (PRD §5.2): `Exec` and
/// `FileChange` may be delegated to operators; `PermissionEscalation` (paths
/// outside the allowlist, network, privilege) always goes to the owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    Exec,
    FileChange,
    PermissionEscalation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub approval_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub kind: ApprovalKind,
    /// Tool name as the harness reports it (`Bash`, `Edit`, `shell`, …).
    pub tool: String,
    /// Tool input, verbatim — the viewer renders the command / diff from it.
    pub input: Value,
    /// Human-readable one-liner: `rm -rf ./build`, `edit src/main.rs (+12 −3)`.
    pub summary: String,
    /// Paths touched, if the tool declares them.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Seconds until the harness applies its default (usually deny).
    pub timeout_secs: u64,
    /// This call reaches beyond what an operator may green-light, so only the
    /// workspace owner may answer it.
    ///
    /// # Why this is a machine-side verdict and not something the hub infers
    ///
    /// The hub cannot work this out. [`ApprovalKind`] is a routing hint, not a
    /// security judgement: a `Bash` command that curls the network carries no
    /// `paths`, so it classifies as [`ApprovalKind::Exec`] — the very category
    /// an operator is allowed to approve. Only the machine can canonicalize the
    /// paths, compare them against the workspace's allowlist, and see the
    /// command line.
    ///
    /// So the machine decides and the hub obeys. A frame that arrives without
    /// this field (older daemon) is treated as owner-only: an unknown verdict
    /// must never read as "safe".
    #[serde(default = "owner_by_default")]
    pub requires_owner: bool,
    /// **Why** this one went back to the owner.
    ///
    /// A boolean cannot carry the difference, and the difference decides
    /// whether the sentence shown to a human is true or false: `Escalates` is
    /// "we saw it reach out" (network tools, `networkApprovalContext`,
    /// `grantRoot`, a path outside the allowlist, a write into `.git/`);
    /// `Unprovable` is "we cannot prove it stays inside" (an unknown tool, a
    /// command line that does not parse, a command name off the list).
    ///
    /// Under this test **the second kind is the norm**: `cargo test` in a bound
    /// repo crosses nothing, and telling the owner "this approval widens what
    /// the agent can touch" is a false sentence. A warning that does not hold
    /// is worse than no warning — it makes the few that really do cross look
    /// like the noise, and the owner starts clicking without reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_reason: Option<OwnerReason>,
    /// The harness offered a "don't ask again this session" form of approval,
    /// so the card may show that third button.
    ///
    /// A capability of *this request*, not of the runtime: claude-code attaches
    /// its `permission_suggestions` per call, and a tool it will never
    /// auto-accept simply arrives without one.
    #[serde(default)]
    pub can_allow_for_session: bool,
    /// The exact neutral mode a session-scoped approval would apply, when the
    /// harness expresses that action as a mode change.
    ///
    /// This is authorization evidence, not just UI metadata: agitd must durably
    /// record a transition to [`PermissionMode::Bypass`] before the supervisor
    /// echoes the native suggestion. `None` is valid for runtimes such as codex
    /// whose session-scoped approval is a native per-request rule rather than a
    /// permission-mode change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_permission_mode: Option<PermissionMode>,
    pub requested_at: String,
}

/// A missing field is owner-only.
///
/// That is what the doc above states, while a bare `#[serde(default)]` yields
/// `false` — the opposite. The hub reads raw JSON, so this field does not bite
/// it, but anywhere `ApprovalRequest` is deserialized back would silently get
/// "no owner needed".
fn owner_by_default() -> bool {
    true
}

/// The two reasons an approval lands back on the owner. See
/// [`ApprovalRequest::owner_reason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerReason {
    /// A positive danger signal is present.
    Escalates,
    /// This call cannot be proven confined to the workspace. Not the same as
    /// dangerous — the same as **unknown**.
    Unprovable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Deny,
}

/// How far one approval reaches.
///
/// `Session` is the "yes, and stop asking" button. It is a separate field
/// rather than a third `ApprovalDecision` variant because it is orthogonal to
/// allow/deny and both harnesses model it that way — claude-code by echoing
/// back a `setMode` permission suggestion, codex by the `acceptForSession`
/// decision value. Folding it into the decision would force every consumer to
/// handle a combination that means nothing (`deny for session`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    /// This call only.
    #[default]
    Once,
    /// This call and every similar one for the rest of the session.
    Session,
}

/// How an approval is answered.
///
/// These are the params of `approval.decide` (viewer → hub → agitd). They are
/// **not** a JSON-RPC response: `approval.request` is a notification with no
/// `id`, so nothing can respond to it. The two halves are paired by
/// `approval_id`, and the machine — not the hub — is what makes the decision
/// stick (it re-checks the caller and the classifier's verdict against its own
/// `pending` table; see `method::APPROVAL_REQUEST`).
///
/// The difference is not wording: someone implementing a client from the old
/// description would wait for an `id` match that never comes, or assume an
/// unanswered request gets retried.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub approval_id: String,
    /// Which session holds the approval. Required because approval ids are
    /// scoped to a live harness, and searching other sessions is both
    /// cross-tenant unsafe and unbounded work under the daemon's global lock.
    pub session_id: String,
    pub decision: ApprovalDecision,
    /// How far the decision reaches. Only meaningful with `Allow`.
    #[serde(default)]
    pub scope: ApprovalScope,
    /// Optional note back to the model ("use git clean instead").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_approval_decision_requires_its_session_route_on_the_wire() {
        let missing = serde_json::json!({
            "approval_id": "approval-1",
            "decision": "allow",
            "scope": "once"
        });
        assert!(
            serde_json::from_value::<ApprovalResponse>(missing).is_err(),
            "the DTO must reject a response the daemon cannot route safely"
        );

        let complete = serde_json::json!({
            "approval_id": "approval-1",
            "session_id": "session-1",
            "decision": "allow",
            "scope": "once"
        });
        assert_eq!(
            serde_json::from_value::<ApprovalResponse>(complete)
                .expect("required route parses")
                .session_id,
            "session-1"
        );
    }

    #[test]
    fn enums_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&Delivery::AtToolBoundary).unwrap(),
            "\"at_tool_boundary\""
        );
        assert_eq!(
            serde_json::to_string(&SessionStatus::AwaitingApproval).unwrap(),
            "\"awaiting_approval\""
        );
        assert_eq!(
            serde_json::to_string(&ApprovalKind::PermissionEscalation).unwrap(),
            "\"permission_escalation\""
        );
    }

    #[test]
    fn item_completed_carries_ir_event_verbatim() {
        let ev = IrEvent::text(
            IrEventKind::ToolUse,
            "Bash",
            Some("2026-08-16T00:00:00Z".into()),
        )
        .at_line(12);
        let raw = serde_json::json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}});
        let hash = crate::domain::transcript::object_hash(&raw);
        let ic = ItemCompleted {
            item_id: "i1".into(),
            turn_id: "t1".into(),
            event: ev.clone(),
            line: 12,
            object_hash: hash.clone(),
            raw: raw.clone(),
            raw_truncated: false,
        };
        let s = serde_json::to_string(&ic).unwrap();
        let back: ItemCompleted = serde_json::from_str(&s).unwrap();
        assert_eq!(back.event.kind, IrEventKind::ToolUse);
        assert_eq!(back.event.line, Some(12));
        assert_eq!(back.object_hash, hash);
        assert_eq!(hash.len(), 40);
    }

    #[test]
    fn older_peer_tolerates_missing_optional_fields() {
        // A register frame from a build that predates `last_seq` / `workspaces`.
        let j = r#"{"protocol_version":1,"machine_fingerprint":"f","display_name":"d","agit_version":"0.9","platform":"linux","capabilities":[]}"#;
        let r: RcRegister = serde_json::from_str(j).unwrap();
        assert!(r.workspaces.is_empty());
        assert!(r.last_seq.is_empty());
    }
}
