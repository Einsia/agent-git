//! Runtime adapter layer.
//!
//! # Why an intermediate representation
//!
//! The two runtimes' on-disk formats are completely different. Without an IR, every feature
//! (rendering, search, sharing, cross-runtime resume) is written twice, and a third runtime
//! means touching all of them.
//!
//! ```text
//!   Claude Code jsonl ──parse──┐
//!                              ├──> Session (IR) ──render──> any format
//!   Codex rollout jsonl ─parse─┘
//! ```
//!
//! # The IR is deliberately small
//!
//! It keeps only what both runtimes can express and what the layers above actually use.
//! Encrypted reasoning and vendor-proprietary tool encodings have no counterpart; they are
//! marked `Other` and dropped in conversion — that loss is **explicit** (it gets reported), not
//! silent.
//!
//! This is also why the store keeps the **raw** jsonl rather than the IR: the IR is a lossy
//! projection, good for rendering and search, unfit as the source of truth. The line in the docs
//! — "any compression or distillation loses the part that is hardest to rebuild, namely 'why not
//! that way'" — is about exactly this.

pub mod claude_code;
pub mod claude_desktop;
pub mod codex;
pub mod codex_index;
pub mod cursor;
pub mod enrich;
pub mod opencode;

use crate::Result;
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const RUNTIMES: &[&str] = &[
    "claude-code",
    "codex",
    "cursor",
    "claude-desktop",
    "opencode",
];

/// Normalize a runtime name typed by the user. Common short forms are accepted because users
/// type both.
///
/// This can normalize to a name with **no registered adapter** (see [`KNOWN_BUT_UNREGISTERED`]) —
/// `get` reports that case as "not supported yet" instead of the catch-all bug message.
pub fn normalize(s: &str) -> Result<&'static str> {
    match s.trim().to_ascii_lowercase().as_str() {
        "claude-code" | "claude" | "cc" => Ok("claude-code"),
        "codex" | "cx" => Ok("codex"),
        // No `cs`-style short form: one letter away from `cc`, so a typo silently goes looking
        // for another runtime's session. `cursor` is short enough not to be worth abbreviating.
        "cursor" => Ok("cursor"),
        // ChatGPT Desktop (com.openai.codex) embeds the Codex engine and reads and writes the
        // same `CODEX_HOME` — it is not another runtime, it is another interface onto the same
        // one, so it normalizes to `codex` instead of getting its own adapter (research in
        // docs/mechanism-probing/desktop-apps.md §3.1).
        "chatgpt-desktop" | "chatgpt" | "chatgpt-app" | "codex-app" => Ok("codex"),
        // Claude Desktop's Code tab writes Claude Code's own jsonl, but its write side hands
        // the file to the desktop app to pick up rather than being directly resumable, so it
        // needs a name of its own (§3.2/§4).
        "claude-desktop" | "claude-app" => Ok("claude-desktop"),
        // No `oc`-style short form: one letter away from `cc` / `cx`, so a typo silently goes
        // looking for another runtime's session (same reason as cursor).
        "opencode" => Ok("opencode"),
        other => anyhow::bail!(
            "unknown runtime `{other}`. Registered: {}",
            RUNTIMES.join(", ")
        ),
    }
}

/// Intermediate representation (IR) of a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub runtime: String,
    /// The runtime's working directory. **The source of truth for which project a session
    /// belongs to.**
    pub cwd: Option<String>,
    pub events: Vec<Event>,
}

impl Session {
    /// The opening prompt, shortened. `agit log` leans on it so the user recognizes "that was
    /// the payments-module one".
    ///
    /// **Only a real `UserPrompt` counts; compact boundaries are skipped.** A compact summary
    /// looks like a user message in both runtimes (Claude Code's `type` and `role` are both
    /// `"user"`), so taking the first user message makes a resumed session show
    /// "This session is being continued from a previous conversation…" instead of the first
    /// thing the user actually typed — which destroys exactly what `agit log` is for.
    ///
    /// Truncation counts `chars()`, not bytes — transcripts are full of CJK text, and a cut on a
    /// byte boundary produces mojibake.
    pub fn gist(&self, max: usize) -> Option<String> {
        let text = self
            .events
            .iter()
            .find(|e| e.kind == EventKind::UserPrompt)
            .and_then(|e| e.text.as_deref())?;
        let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut s: String = one_line.chars().take(max).collect();
        if one_line.chars().count() > max {
            s.push('…');
        }
        Some(s)
    }

    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for e in &self.events {
            match e.kind {
                EventKind::UserPrompt => c.prompts += 1,
                // Interjections are counted on their own and **not folded into prompts**:
                // prompts correspond to turns, and an interjection belongs to the turn that
                // is already open.
                EventKind::UserInterjection => c.interjections += 1,
                EventKind::AssistantReply => c.replies += 1,
                EventKind::ToolUse => c.tools += 1,
                EventKind::FileEdit => c.edits += 1,
                // Tool output is counted on its own and **not folded into tools**: `tools`
                // answers "how many commands ran". With one output per command the two move
                // together, but failed retries and multi-block output pull them apart. Merging
                // them inflates the "how many commands ran" number.
                EventKind::ToolResult => c.outputs += 1,
                // Compact boundaries are counted on their own and **not folded into prompts**.
                //
                // Folding them in inflates counts().prompts: one prompt that survives repeated
                // compaction is counted once per compaction, and the "N prompts" activity
                // summary reads off that number.
                EventKind::CompactFiltered | EventKind::CompactSummary => c.compactions += 1,
                // A terminator marker is not content; it enters no activity count.
                EventKind::TurnEnd => {}
                EventKind::Other => c.dropped += 1,
            }
        }
        c
    }

    pub fn last_timestamp(&self) -> Option<&str> {
        self.events
            .iter()
            .rev()
            .find_map(|e| e.timestamp.as_deref())
    }
}

#[derive(Debug, Clone, Default)]
pub struct Counts {
    pub prompts: usize,
    /// User messages that arrive while a turn is in progress ([`EventKind::UserInterjection`]).
    pub interjections: usize,
    pub replies: usize,
    pub tools: usize,
    pub edits: usize,
    /// How many tool outputs there are ([`EventKind::ToolResult`]).
    ///
    /// Kept apart from `tools`: see the note inside `counts()`.
    pub outputs: usize,
    /// How many events the IR cannot express and conversion therefore drops.
    pub dropped: usize,
    /// How many times context compaction happened.
    ///
    /// The number is diagnostic on its own — it says how often the session reached the context
    /// limit.
    pub compactions: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub kind: EventKind,
    pub text: Option<String>,
    pub timestamp: Option<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    pub tool: Option<String>,
    /// Which line of the transcript this event came from (0-based).
    ///
    /// # Why the IR carries a coordinate pointing back at the source
    ///
    /// The IR is a **lossy** projection: tool arguments, tool output, thinking bodies and diffs
    /// are all inexpressible. A transcript page that shows "→ called Bash" without the command
    /// itself is worth far less than it should be.
    ///
    /// There are two ways to fill that in. One is to push all of it into the IR — that defeats
    /// the "the IR is deliberately small" intent, and every runtime-proprietary field then means
    /// touching the IR. The other is for each event to remember which line it came from, and for
    /// a consumer that needs detail to take that coordinate and **go back to the raw jsonl for
    /// it**.
    ///
    /// The second: the raw jsonl stays the single source of truth, the IR stays limited to the
    /// semantics both runtimes share, and "expand this tool call's arguments" becomes a re-parse
    /// of the same line.
    ///
    /// One line can produce several events (Claude Code's assistant records are block arrays), so
    /// the line number is **not unique** — a consumer pairs against the blocks in the source by
    /// "order among same-kind events within one line".
    ///
    /// It takes no part in [`crate::domain::turn`] normalization: a line number is a position,
    /// not content, and re-rendering the session elsewhere necessarily changes it; mixing it into
    /// the hash destroys the ability to recognize one turn across machines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
}

impl Event {
    pub fn text(kind: EventKind, text: impl Into<String>, ts: Option<String>) -> Event {
        Event {
            kind,
            text: Some(text.into()),
            timestamp: ts,
            paths: vec![],
            tool: None,
            line: None,
        }
    }

    /// Record which line this event came from.
    pub fn at_line(mut self, line: usize) -> Event {
        self.line = Some(line);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    UserPrompt,
    /// Something the user says while a turn is **in progress**.
    ///
    /// Claude Code lets the user keep typing while the agent is still running tools: that line
    /// does not wait for the next turn, it is "absorbed" into the current one — the transcript
    /// writes no `user` record for it, only a `queue-operation` `remove`
    /// (`reason: absorbed_mid_turn`) holding the original text, plus a copy paraphrased by the
    /// runtime inside the next `tool_result` body.
    ///
    /// It is a person speaking, so it must reach the log and the VIEW (otherwise a resumed agent
    /// does not know the user changed the ask mid-flight); but it **opens no new turn**: the
    /// model read and answered it within the same turn, and cutting there splits one turn into
    /// two halves that each lack the other. So it is a variant of its own rather than a flagged
    /// `UserPrompt` — turn splitting recognizes only `UserPrompt`, while every renderer emits
    /// this as a user message.
    UserInterjection,
    AssistantReply,
    ToolUse,
    FileEdit,

    // ── The two shapes of compaction ─────
    //
    // Deliberately two variants rather than one variant plus `lossy: bool`: `match` forces every
    // consumer to handle both cases explicitly (dedupe or not, how to render, whether it can be
    // fed to merge), and the compiler makes them think it through. A boolean flag is easy to
    // ignore.
    //
    // The two runtimes' compaction mechanisms are **fundamentally different** (observed; see
    // notes/compact-mechanism.md):
    /// Codex's compact: **lossless filtering**.
    ///
    /// The `replacement_history` of a `compacted` record is a structured message array; user
    /// input is **kept verbatim** (observed: every user input matched exactly), and only
    /// assistant output, tool calls and reasoning are dropped. Windows chain into a traceable
    /// list through `window_number` + `previous_window_id`.
    ///
    /// Consequence: a search hit here is a hit on the original text (merely duplicated), and
    /// merge can take it as input directly.
    CompactFiltered,

    /// Claude Code's compact: **lossy summarization**.
    ///
    /// A synthetic user message carrying `isCompactSummary: true`, with the user's own words
    /// kneaded into one continuous block of summary text, and `parentUuid: null` breaking the
    /// message chain.
    ///
    /// Consequence: a search hit here is second-hand and has to be labelled or pointed back at
    /// the original; **it must never be fed to merge** — that would take the summary for the
    /// user's original intent.
    CompactSummary,

    /// The runtime declaring "this turn is done".
    ///
    /// Codex writes one `event_msg/task_complete` at the end of every turn — a turn still
    /// running has none yet. **Claude Code has no equivalent**: its `system/turn_duration` does
    /// not line up with the human turn count, in either direction, and it also fires for
    /// interruptions and system turns.
    ///
    /// It is modelled only so it is **not mistaken for content**: rendering skips it, activity
    /// counts must not score it as a reply, and turn normalization excludes it (otherwise the
    /// same conversation hashes differently under the two runtimes).
    ///
    /// It **does not affect versions**. Deriving the version from the last closed turn would
    /// mean the Claude Code side never records its trailing turn; the version comes from the
    /// snapshot `agit commit` takes, so no terminator signal is needed.
    TurnEnd,

    /// **What a tool spat out** (`tool_result` / `function_call_output`).
    ///
    /// # Why it earns a variant instead of being folded into `ToolUse`
    ///
    /// `ToolUse` answers "did anyone run this command", this one answers "did the command ever
    /// emit this value". Two different questions: the first is intent, the second is fact. In a
    /// real corpus, a large share of the distinct config-assignment forms appear only in tool
    /// output — the output of `cat config.yaml` and of `grep lr:`. Folding it into `ToolUse`
    /// drowns `in:tool` results in log noise, and the whole value of that qualifier is "someone
    /// really did run this command".
    ///
    /// # Why it is not left to the text extractor
    ///
    /// `claude_code::extract_text` takes only `type == "text"` blocks, so without a variant of
    /// its own a `tool_result` record produces **no** IR event at all — not recorded as `Other`,
    /// simply absent. It would then show up neither in the dropped count nor in a search. Such
    /// blocks are common in a real corpus, and every one of them would be lost with no symptom.
    ///
    /// # The three places it takes no part in, each for its own reason
    ///
    /// - **Turn splitting**: cuts happen only at `UserPrompt`. The module header
    ///   ([`crate::domain::turn`]) holds the observed data: treating tool feedback as a turn
    ///   boundary explodes the turn count of a CC session while a Codex session of the same size
    ///   stays tiny — one concept differing by orders of magnitude between the two.
    /// - **Turn hash**: same reason as `TurnEnd`, only harder. A CC session carries a great many
    ///   `tool_result` records, and Codex's `function_call_output` differs in both shape and
    ///   granularity; including them makes the cross-runtime hash necessarily differ, and
    ///   recognizing "the same conversation" is the turn hash's only job.
    /// - **`render`**: it must **never** be emitted as a user message. That makes a resumed agent
    ///   believe the user pasted a whole block of command output in by hand. The right place for
    ///   tool output is the paired `tool_use_id`, and the IR does not model that pairing (see
    ///   [`Event::line`]: go back to the source for detail).
    ToolResult,

    /// Present in the source format but not modelled by the IR (encrypted reasoning,
    /// vendor-proprietary encodings). Kept so the loss can be **reported**, never rendered.
    Other,
}

impl EventKind {
    /// Whether this is a compact boundary (either shape counts).
    pub fn is_compact(self) -> bool {
        matches!(self, EventKind::CompactFiltered | EventKind::CompactSummary)
    }

    /// Whether the content is a lossy summary — this decides whether it may stand for the
    /// user's original intent.
    ///
    /// `merge` leans on it: Codex's filtered result is verbatim original text and goes straight
    /// in as input; Claude Code's summary is second-hand, and feeding it in contaminates the
    /// reconciliation.
    pub fn is_lossy_summary(self) -> bool {
        matches!(self, EventKind::CompactSummary)
    }
}

/// Where a session lives on disk, plus its metadata.
///
/// It carries no full content: listing needs metadata only, and fully parsing a large transcript
/// is expensive.
#[derive(Debug, Clone)]
pub struct SessionRef {
    pub id: String,
    pub path: PathBuf,
    pub runtime: &'static str,
    pub cwd: Option<String>,
    pub mtime: std::time::SystemTime,
    /// The opening prompt, shortened, **when the runtime's own index already carries it**.
    ///
    /// Codex's `threads` table has `first_user_message` for free; dropping it means the caller
    /// re-parses a transcript for every single session, and that per-session cost multiplies by
    /// the number of sessions. Claude Code has no equivalent index, so it stays `None` there and
    /// the caller decides whether to pay for it, and for how many.
    pub gist: Option<String>,
}

/// A tool call that was issued but has no paired output in the transcript yet.
///
/// # Why it is not in the IR
///
/// The IR deliberately does not model the "call ↔ output" pairing (see
/// [`EventKind::ToolResult`]): that is each runtime's own protocol detail, with no shape in
/// common across runtimes. But "is this turn still running" is exactly the question only the
/// pairing can answer — the model issued a call, the result has not been written back, so the
/// turn has not ended. The pairing decision therefore stays in the adapter, made against each
/// runtime's own records, and only "which calls are still open" is handed out.
///
/// `line` is that call record's 0-based line number in the transcript, in the same coordinate
/// system as [`Event::line`]; settlement uses it to decide which turn an open call falls in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenCall {
    pub line: usize,
    /// The runtime's own call id (Codex's `call_id`, Claude Code's `tool_use.id`).
    pub call_id: String,
    /// The call's record type (`function_call` / `custom_tool_call` / `tool_use` ...); closing
    /// it writes the output record in the same family.
    pub record: String,
    pub name: String,
}

/// How far agit can go with a target runtime.
///
/// Deliberately one enum rather than several bools: a combination of bools produces meaningless
/// states (installable but not parseable), while `match` forces every consumer to handle each
/// level explicitly — the same reason `EventKind` splits into two compact variants instead of
/// carrying a `lossy: bool`.
///
/// The order is meaningful (`Ord`): picking "the best one" is a comparison.
///
/// Design source: docs/mechanism-probing/desktop-apps.md §4.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    /// Read in only. It cannot be installed into, and `--as` refuses it.
    ///
    /// Example: Cursor — the transcript is a projection rather than the source of truth (the
    /// source of truth is an encrypted protobuf inside `state.vscdb`), and there is no resumable
    /// CLI entry point.
    ImportOnly,

    /// It can produce a real deliverable (a file plus one hand-off instruction), but the last
    /// step happens in a process agit cannot observe, so **agit cannot claim success**.
    ///
    /// Example: Claude Desktop — the transcript goes into `~/.claude/projects/`, and pickup is
    /// handed to the app itself through `claude://resume?session=<uuid>`.
    ExportOnly,

    /// It can be installed to disk and yields a resume command that is certain to run.
    Resumable,
}

impl Capability {
    /// The one-word label in `doctor`'s runtime row.
    pub fn label(self) -> &'static str {
        match self {
            Capability::ImportOnly => "read-only",
            Capability::ExportOnly => "export-only",
            Capability::Resumable => "resumable",
        }
    }

    /// The next-step hint in `doctor`'s runtime row — the "so what now" for each capability
    /// level.
    ///
    /// Only non-`Resumable` rows need it (`Resumable`'s next step is the command itself and needs
    /// no explanation). The wording must be an **instruction someone can follow**, never a "this
    /// might work" that leaves no next step.
    pub fn next_hint(self) -> &'static str {
        match self {
            Capability::ImportOnly => "importable, but not a resume target",
            Capability::ExportOnly => {
                "after installing, the target app picks it up itself — agit cannot observe the result"
            }
            // Not used in a doctor row; it is here only so every arm of the match has settled
            // wording.
            Capability::Resumable => "after installing, yields a directly runnable resume command",
        }
    }
}

/// Runtime adapter.
/// Source-side detail of one tool call ([`enrich`] fetches it from the source transcript by
/// `Event.line`).
#[derive(Debug, Clone, Default)]
pub struct ToolDetail {
    /// Call arguments, in whatever shape the source holds them (an object, or the raw string
    /// when it does not parse).
    pub input: Option<serde_json::Value>,
    /// The paired output body (the source pairs by call id before it gets here; order is never
    /// guessed).
    ///
    /// `Some("")` and `None` are two different facts: the first is "the tool returned
    /// successfully with no output", the second is "the output was not retrieved" — the renderer
    /// places placeholder text only for the second.
    pub output: Option<String>,
    /// The source records this call as failed (Claude's `is_error`, OpenCode's `status=="error"`).
    pub error: bool,
}

/// Tool detail table, indexed by **event index** (into `session.events`).
///
/// Detail deliberately stays out of the IR (the comment on `Event::line` is where that decision
/// lives): the IR expresses shared semantics only, and a consumer goes back to the source for
/// detail at the moment it needs it. This table is the carrier for "fetched, now hand it to
/// render", and cross-runtime installation ([`crate::domain::install`]) is its only filler.
#[derive(Debug, Clone, Default)]
pub struct ToolDetails {
    map: std::collections::HashMap<usize, ToolDetail>,
    /// Indices of receipt-shaped events: they are a projection of another call (the Codex source
    /// projects patch_apply_end into a FileEdit while the real call has a ToolUse event of its
    /// own), and the renderer keeps the change signal only rather than minting a second call.
    /// Only the source extractor can tell them apart — away from the source format, a "FileEdit
    /// with no detail" is either a receipt or a real call whose enrichment failed.
    receipts: std::collections::HashSet<usize>,
}

impl ToolDetails {
    pub fn insert(&mut self, event_idx: usize, detail: ToolDetail) {
        self.map.insert(event_idx, detail);
    }
    pub fn get(&self, event_idx: usize) -> Option<&ToolDetail> {
        self.map.get(&event_idx)
    }
    pub fn mark_receipt(&mut self, event_idx: usize) {
        self.receipts.insert(event_idx);
    }
    pub fn is_receipt(&self, event_idx: usize) -> bool {
        self.receipts.contains(&event_idx)
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty() && self.receipts.is_empty()
    }
}

pub trait Adapter {
    fn id(&self) -> &'static str;

    /// The executable's name.
    fn cli(&self) -> &'static str;

    /// Which level this target reaches.
    ///
    /// **Deliberately no default implementation.** With a `Resumable` default, an adapter whose
    /// author forgets to override it is swept into `clone`'s default fan-out and produces a
    /// session that installs but never starts — precisely the failure shape this capability model
    /// exists to remove. One extra line buys the compiler remembering it for you.
    fn capability(&self) -> Capability;

    /// The transcript's on-disk format family. **It decides whether installing back into its
    /// own family can go through byte rewriting.**
    ///
    /// Kept separate from `id()` because several runtimes share one format: Claude Desktop's Code
    /// tab writes Claude Code's own jsonl, and ChatGPT Desktop's Codex side writes Codex's own
    /// rollout. Comparing by `id()` judges them cross-vendor and throws away encrypted reasoning
    /// for nothing (desktop-apps.md §4.2).
    fn format(&self) -> &'static str;

    /// List the sessions that belong to a repo.
    ///
    /// The test for "belongs" is the cwd the session records, not which directory the file sits
    /// in — Codex splits directories by date and the path carries no project information.
    fn sessions_for(&self, repo: &Path) -> Result<Vec<SessionRef>>;

    /// Look a transcript file up by session id.
    ///
    /// **This is the foundation of the session-link design.** The store records only
    /// `(source, session_id, cwd)`, so every read of the content starts with a lookup. Both sides
    /// are cheap in practice: Codex queries the `threads` table (falling back to a glob by id
    /// when the database is unavailable, since the filename embeds the id); Claude Code needs one
    /// glob level, `projects/*/<id>.jsonl`.
    ///
    /// `cwd` is an **optional fast path**: Claude Code's project directory name is a slug of the
    /// cwd, so passing it goes straight there. The slug rule is not one we can guarantee, so a
    /// failed direct hit must fall back to the lookup by id.
    fn resolve(&self, session_id: &str, cwd: Option<&Path>) -> Option<PathBuf>;

    /// List **every** session of this runtime (not limited to one repo).
    ///
    /// For `doctor` to check for missed captures. More expensive than `sessions_for`.
    fn all_sessions(&self) -> Result<Vec<SessionRef>>;

    fn parse(&self, text: &str) -> Result<Session>;

    /// Parse from a **path**.
    ///
    /// # Why `parse(&str)` alone is not enough
    ///
    /// A Cursor transcript has neither `sessionId` nor `cwd` — both exist only in the path
    /// (`~/.cursor/projects/<slug>/agent-transcripts/<uuid>/<uuid>.jsonl`). Parsing a bare string
    /// **cannot** yield `Session::id` and `Session::cwd`.
    ///
    /// The default implementation reads the file and hands it to `parse`, so for Claude Code and
    /// Codex it is pure addition: their identity fields are in the body already, and where the
    /// body is read from makes no difference.
    ///
    /// Code that already holds the body need not come through here (a backend with a blob and no
    /// path, say) — identity comes from elsewhere in that case and `parse` is enough.
    fn parse_at(&self, path: &Path) -> Result<Session> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        self.parse(&text)
    }

    /// Render into a format this runtime can resume from.
    fn render(&self, session: &Session, new_id: &str, cwd: &Path) -> Result<String>;

    /// Render, carrying the tool detail fetched from the source transcript (arguments and paired
    /// output, see [`enrich`]).
    ///
    /// The default implementation ignores the detail and goes straight to [`Adapter::render`]:
    /// not every target replays tool calls (cursor is import-only, desktop goes through
    /// same-family byte rewriting), so they do not pay for an implementation.
    fn render_with(
        &self,
        session: &Session,
        new_id: &str,
        cwd: &Path,
        _details: &ToolDetails,
    ) -> Result<String> {
        self.render(session, new_id, cwd)
    }

    /// Tool calls issued in the transcript with no paired output yet, in order of appearance.
    ///
    /// Settlement uses it to decide whether the trailing turn is still in progress: a call whose
    /// result has not been written back means the turn has not ended and must not be cut.
    /// Installing back into a runtime uses it too, to give a dangling call a placeholder output —
    /// otherwise the resumed session carries a call whose result never arrives.
    ///
    /// Empty by default — better a runtime that does not know the pairing protocol report "no
    /// open calls" than guess.
    fn open_tool_calls(&self, _text: &str) -> Vec<OpenCall> {
        Vec::new()
    }

    /// Mint a new id this runtime accepts.
    fn mint_id(&self) -> String;

    /// Write to the location the runtime scans.
    fn install(&self, content: &str, new_id: &str, cwd: &Path) -> Result<Installed>;

    /// Whether this runtime can be installed into (= whether it can be a `resume` / `clone`
    /// target).
    ///
    /// Derived from [`Adapter::capability`]: `ImportOnly` cannot, the rest can. This gate is only
    /// a projection of the capability model; to decide "can it be installed into", read
    /// `capability()` rather than opening another boolean bypass.
    ///
    /// **Callers above must check it before starting work**, or this system's worst failure shape
    /// follows — the command returns successfully and the user opens the runtime to nothing.
    fn installable(&self) -> bool {
        self.capability() != Capability::ImportOnly
    }

    fn available(&self) -> bool {
        which(self.cli()).is_some()
    }
}

#[derive(Debug, Clone)]
pub struct Installed {
    pub path: PathBuf,
    /// What to do once the install is done.
    pub next: Next,
}

/// What to do once the install is done (desktop-apps.md §4.2).
#[derive(Debug, Clone)]
pub enum Next {
    /// Run this command to resume, directly.
    Resume(String),
    /// agit stops here. The two fields differ in nature and must be presented apart: `trigger`
    /// can fail where agit cannot observe it, while `fallback` is certain to work. Both must be
    /// **steps someone can follow**, never a "you may need to import it by hand" that leaves no
    /// next step.
    HandOff { trigger: String, fallback: String },
}

/// Canonical names normalize knows but for which no adapter is registered.
///
/// The table exists for one reason: to keep "not supported yet" apart from "bug". Without it,
/// `get`'s catch-all branch reports a runtime that is **planned but unwritten** as "no adapter
/// (bug)" — and the user concludes the install is broken rather than the feature unbuilt.
///
/// Empty: `claude-desktop` ships with the capability model (S10, desktop-apps.md §4). The next
/// runtime that normalize knows but has no adapter goes back into this table.
const KNOWN_BUT_UNREGISTERED: &[&str] = &[];

pub fn get(runtime: &str) -> Result<Box<dyn Adapter>> {
    match normalize(runtime)? {
        "claude-code" => Ok(Box::new(claude_code::ClaudeCode)),
        "codex" => Ok(Box::new(codex::Codex)),
        "cursor" => Ok(Box::new(cursor::Cursor)),
        "claude-desktop" => Ok(Box::new(claude_desktop::ClaudeDesktop)),
        "opencode" => Ok(Box::new(opencode::OpenCode)),
        other if KNOWN_BUT_UNREGISTERED.contains(&other) => {
            anyhow::bail!(
                "runtime `{other}` is not supported yet: normalize knows it, but no adapter implements it"
            )
        }
        other => anyhow::bail!("runtime `{other}` has no adapter (bug)"),
    }
}

pub fn all() -> Vec<Box<dyn Adapter>> {
    vec![
        Box::new(claude_code::ClaudeCode),
        Box::new(codex::Codex),
        Box::new(cursor::Cursor),
        // Export-only: the capability model keeps it out of the default resume list, and only
        // an explicit `--as` reaches it.
        Box::new(claude_desktop::ClaudeDesktop),
        Box::new(opencode::OpenCode),
    ]
}

/// Which runtimes `clone` installs into by default when no `--as` is given.
///
/// The test is **capability**, not a hand-written list (desktop-apps.md §4.3): adding a runtime
/// later needs no edit here, and forgetting one cannot sweep an unresumable target into the
/// default fan-out.
pub fn default_targets() -> Vec<&'static str> {
    let mut v: Vec<_> = all()
        .into_iter()
        .filter(|a| a.capability() == Capability::Resumable)
        .map(|a| a.id())
        .collect();
    v.sort();
    v
}

/// The list of non-default targets (so `clone` can mention at the end that they exist).
///
/// Returns `(id, capability)` — a name without the reason it does not qualify leaves the user
/// still not knowing what the option is.
pub fn non_default_targets() -> Vec<(&'static str, Capability)> {
    let mut v: Vec<_> = all()
        .into_iter()
        .filter(|a| a.capability() != Capability::Resumable)
        .map(|a| (a.id(), a.capability()))
        .collect();
    v.sort();
    v
}

/// Whether converting from `from` to `to` is lossy (desktop-apps.md §4.3).
///
/// The test is the **transcript format family**, not the runtime name. The same format goes
/// through byte rewriting (`domain::install::rewrite_identity`), and encrypted reasoning and
/// compact boundaries all pass through unchanged. Several runtimes share one format family
/// (`claude-desktop` writes Claude Code's own jsonl) — comparing by runtime name misjudges them
/// as lossy and throws away encrypted reasoning for nothing.
pub fn is_lossy_conversion(from: &str, to: &str) -> bool {
    match (get(from), get(to)) {
        (Ok(a), Ok(b)) => a.format() != b.format(),
        // An unrecognized name counts as lossy. Better one loss reported too many than one missed.
        _ => true,
    }
}

/// Find an executable on PATH. Implemented here to drop a dependency.
pub fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(cmd))
        .find(|p| p.is_file())
}

/// Take the session id out of a transcript filename.
///
/// A Codex filename carries a timestamp prefix:
/// `rollout-2026-07-25T18-20-01-<uuid>` → `<uuid>` (the uuid is the last five `-`-separated
/// segments). Any other shape is returned unchanged (a Claude Code filename is the id).
pub fn session_id_from_stem(stem: &str) -> String {
    if !stem.starts_with("rollout-") {
        return stem.to_string();
    }
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 5 {
        return stem.to_string();
    }
    parts[parts.len() - 5..].join("-")
}

/// Guess the source runtime from the content.
///
/// The test is the fields unique to each format, not the filename — a file may have been renamed
/// or copied in from elsewhere.
///
/// # Why it scans the first few lines instead of the first line alone
///
/// A Cursor transcript's first line may be `turn_ended` (a terminator record with neither `role`
/// nor `message`); reading the first line alone yields None and the caller falls back to the
/// runtime recorded in the link — which fails exactly on "the file was moved", the one reason
/// `infer_runtime` exists.
///
/// # Order of the tests
///
/// **Cursor must come after Claude Code.** Records from both carry `message`; the only difference
/// is that Claude Code also has `sessionId` / `parentUuid`. The other order reads Claude Code as
/// Cursor, and the whole session then parses to zero events.
pub fn infer_runtime(text: &str) -> Option<&'static str> {
    for line in text.lines().filter(|l| !l.trim().is_empty()).take(8) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        // A Codex line is {type, payload}
        if v.get("payload").is_some() && v.get("type").is_some() {
            return Some("codex");
        }
        // A Claude Code line has sessionId / parentUuid
        if v.get("sessionId").is_some() || v.get("parentUuid").is_some() {
            return Some("claude-code");
        }
        // Cursor has only two shapes: {role, message} and {type:"turn_ended", ...}.
        if (v.get("role").is_some() && v.get("message").is_some())
            || v.get("type").and_then(|x| x.as_str()) == Some("turn_ended")
        {
            return Some("cursor");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_aliases() {
        assert_eq!(normalize("claude").unwrap(), "claude-code");
        assert_eq!(normalize(" CC ").unwrap(), "claude-code");
        assert_eq!(normalize("codex").unwrap(), "codex");
        assert!(normalize("gpt").is_err());
    }

    /// Desktop app aliases: ChatGPT Desktop is Codex and must be an alias rather than a new
    /// runtime; Claude Desktop keeps a name of its own (its write side is a hand-off, a
    /// different capability from a CLI target).
    #[test]
    fn normalize_accepts_desktop_aliases() {
        for a in ["chatgpt-desktop", "chatgpt", "chatgpt-app", "codex-app"] {
            assert_eq!(normalize(a).unwrap(), "codex", "{a} normalizes to codex");
        }
        for a in ["claude-desktop", "claude-app"] {
            assert_eq!(
                normalize(a).unwrap(),
                "claude-desktop",
                "{a} normalizes to claude-desktop"
            );
        }
    }

    /// Every name normalize knows must have an adapter.
    ///
    /// KNOWN_BUT_UNREGISTERED is empty, so every name here resolves. That table keeps "not
    /// supported yet" apart from "bug": the next runtime normalize knows but has no adapter for
    /// goes there instead of letting get's catch-all branch print something misleading.
    #[test]
    fn every_normalized_name_has_an_adapter() {
        for name in [
            "claude-code",
            "claude",
            "cc",
            "codex",
            "cx",
            "cursor",
            "chatgpt-desktop",
            "chatgpt",
            "chatgpt-app",
            "codex-app",
            "claude-desktop",
            "claude-app",
            "opencode",
        ] {
            assert!(
                get(name).is_ok(),
                "normalize knows {name} but no adapter implements it"
            );
        }
    }

    /// The lossiness test is the **format family**, not the runtime name (desktop-apps.md §4.3).
    ///
    /// One format family goes through byte rewriting and encrypted reasoning passes unchanged;
    /// only crossing families goes through the IR. Aliases must be normalized before formats are
    /// compared, or `claude`→`cc` is misjudged as lossy.
    #[test]
    fn lossy_judged_by_format_family_not_runtime_name() {
        assert!(is_lossy_conversion("codex", "claude-code"));
        assert!(
            !is_lossy_conversion("claude", "cc"),
            "normalized aliases land in one family, so nothing is lost"
        );
        assert!(
            !is_lossy_conversion("codex", "chatgpt-desktop"),
            "a desktop alias is in the same family"
        );
        assert!(is_lossy_conversion("codex", "cursor"));
        // Claude Desktop and Claude Code write one format family — installing either way goes
        // through byte rewriting and synthesizes no sidecar (desktop-apps.md §4.3/§5.1).
        assert!(
            !is_lossy_conversion("claude-code", "claude-desktop"),
            "one format family"
        );
        assert!(
            is_lossy_conversion("chatgpt-desktop", "claude-desktop"),
            "the codex family and the claude-code family are lossy between them"
        );
    }

    /// An unrecognized name counts as lossy — better one loss reported too many than one missed.
    #[test]
    fn an_unrecognizable_runtime_is_treated_as_lossy() {
        assert!(is_lossy_conversion("codex", "no-such-runtime"));
        assert!(is_lossy_conversion("no-such-runtime", "codex"));
    }

    /// The declaration table from §4.3: every adapter's capability / format must match it.
    /// A new adapter changes this table first and is implemented second — it pins the code
    /// against the research document.
    #[test]
    fn adapters_declare_the_capability_table_from_the_doc() {
        let want: &[(&str, Capability, &str)] = &[
            ("claude-code", Capability::Resumable, "claude-code"),
            ("codex", Capability::Resumable, "codex"),
            // A Cursor transcript is a projection (the truth is an encrypted protobuf in
            // state.vscdb); it goes in, never out.
            ("cursor", Capability::ImportOnly, "cursor"),
            // The desktop Code tab writes Claude Code jsonl (same family, so byte rewriting
            // works), but pickup is the app's own job — export-only.
            ("claude-desktop", Capability::ExportOnly, "claude-code"),
            // §8.3: the official export→import byte path plus `opencode --session` resume in
            // place — the install lands in the source-of-truth store itself, not through a
            // private index nobody can observe.
            ("opencode", Capability::Resumable, "opencode"),
        ];
        let got: Vec<_> = all()
            .iter()
            .map(|a| (a.id(), a.capability(), a.format()))
            .collect();
        assert_eq!(
            got, want,
            "the capability declaration table must match desktop-apps.md §4.3"
        );
    }

    /// The order is "which one is better" (§4.2): picking the best target is an `Ord` comparison.
    #[test]
    fn capability_order_is_importonly_exportonly_resumable() {
        assert!(Capability::ImportOnly < Capability::ExportOnly);
        assert!(Capability::ExportOnly < Capability::Resumable);
    }

    /// The default fan-out is derived from capability (§4.3): all Resumable, stably sorted, and
    /// complementary to `non_default_targets` with nothing missing.
    #[test]
    fn default_targets_are_exactly_the_resumable_ones() {
        let defaults = default_targets();
        assert_eq!(defaults, ["claude-code", "codex", "opencode"]);
        let mut rest: Vec<_> = non_default_targets().iter().map(|(id, _)| *id).collect();
        rest.sort();
        assert_eq!(rest, ["claude-desktop", "cursor"]);
        // The union == every registered adapter, none missing.
        assert_eq!(defaults.len() + rest.len(), all().len());
    }

    /// installable is a projection of capability (§4.2), not a second hand-written switch.
    /// Cursor (ImportOnly) is the only one that cannot be installed into, and this pins that gate.
    #[test]
    fn installable_is_derived_from_capability() {
        for a in all() {
            assert_eq!(
                a.installable(),
                a.capability() != Capability::ImportOnly,
                "{}'s installable() must be derived from capability()",
                a.id()
            );
        }
    }

    #[test]
    fn gist_truncates_by_chars_not_bytes() {
        let s = Session {
            id: "x".into(),
            runtime: "codex".into(),
            cwd: None,
            // CJK truncation fixture (AGENTS.md exception iii): this pins that `gist` cuts on
            // character boundaries, so the sample stays Chinese.
            events: vec![Event::text(
                EventKind::UserPrompt,
                "帮我重构这个模块的错误处理",
                None,
            )],
        };
        let g = s.gist(5).unwrap();
        assert_eq!(g.chars().count(), 6, "5 characters plus the ellipsis: {g}");
        assert!(g.ends_with('…'));
    }

    #[test]
    fn codex_filename_yields_the_uuid() {
        assert_eq!(
            session_id_from_stem(
                "rollout-2026-07-25T18-20-01-019fa89f-0452-7b93-8efd-b494b0d17d0b"
            ),
            "019fa89f-0452-7b93-8efd-b494b0d17d0b"
        );
        // A Claude Code filename is the id.
        assert_eq!(
            session_id_from_stem("d7a15e18-c86b-47ea-bf78-d8a25f4bd18f"),
            "d7a15e18-c86b-47ea-bf78-d8a25f4bd18f"
        );
        // An unrecognized shape is returned unchanged; never cut blindly.
        assert_eq!(session_id_from_stem("weird"), "weird");
        assert_eq!(session_id_from_stem("rollout-short"), "rollout-short");
    }

    #[test]
    fn infer_distinguishes_formats() {
        assert_eq!(
            infer_runtime(r#"{"type":"session_meta","payload":{"cwd":"/x"}}"#),
            Some("codex")
        );
        assert_eq!(
            infer_runtime(r#"{"sessionId":"a","message":{"role":"user"}}"#),
            Some("claude-code")
        );
        assert_eq!(
            infer_runtime(r#"{"role":"user","message":{"content":[]}}"#),
            Some("cursor")
        );
        assert_eq!(infer_runtime("not json"), None);
        assert_eq!(infer_runtime(""), None);
    }

    /// The Cursor test must come after Claude Code.
    ///
    /// Records from both carry `message`; the only difference is that Claude Code also has
    /// `sessionId`. The other order reads Claude Code as Cursor and the whole session parses to
    /// zero events — **silently**, because neither `parse` errors.
    #[test]
    fn claude_code_is_not_mistaken_for_cursor() {
        // A real Claude Code record: the top level carries both message and sessionId.
        let cc = r#"{"type":"user","sessionId":"s","message":{"role":"user","content":"hi"}}"#;
        assert_eq!(infer_runtime(cc), Some("claude-code"));
    }

    /// A Cursor transcript's first line may be `turn_ended`; reading the first line alone
    /// misses it.
    #[test]
    fn infer_scans_past_a_leading_turn_ended() {
        let text = concat!(
            r#"{"type":"turn_ended","status":"success"}"#,
            "\n",
            r#"{"role":"user","message":{"content":[{"type":"text","text":"hi"}]}}"#,
        );
        assert_eq!(infer_runtime(text), Some("cursor"));
        // One lone turn_ended is enough — the shape is unique to Cursor.
        assert_eq!(
            infer_runtime(r#"{"type":"turn_ended","status":"error","error":"x"}"#),
            Some("cursor")
        );
    }

    /// `parse_at`'s default implementation is pure addition for the existing two: read the
    /// file, then go through `parse`.
    #[test]
    fn parse_at_defaults_to_reading_the_file() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("s.jsonl");
        std::fs::write(
            &f,
            r#"{"type":"user","sessionId":"s1","cwd":"/repo","message":{"role":"user","content":"hi"}}"#,
        )
        .unwrap();
        let s = get("claude-code").unwrap().parse_at(&f).unwrap();
        assert_eq!(s.id, "s1");
        assert_eq!(s.cwd.as_deref(), Some("/repo"));
    }

    /// Cursor alone refuses installation — this pins the direction of
    /// [`Capability::ImportOnly`]: a new adapter that forgets to implement `capability()` does
    /// not compile (the trait has no default).
    #[test]
    fn only_cursor_refuses_installation() {
        for a in all() {
            assert_eq!(
                a.installable(),
                a.id() != "cursor",
                "{} is installable exactly when it is not cursor",
                a.id()
            );
        }
    }

    #[test]
    fn counts_track_dropped_events() {
        // The dropped count must be visible — being lossy has to be reportable.
        let s = Session {
            id: "x".into(),
            runtime: "codex".into(),
            cwd: None,
            events: vec![
                Event::text(EventKind::UserPrompt, "q", None),
                Event {
                    kind: EventKind::Other,
                    text: None,
                    timestamp: None,
                    paths: vec![],
                    tool: None,
                    line: None,
                },
            ],
        };
        let c = s.counts();
        assert_eq!(c.prompts, 1);
        assert_eq!(c.dropped, 1, "events the IR cannot express must be counted");
    }
}
