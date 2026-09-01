//! Claude Code adapter.
//!
//! # On-disk format
//!
//! ```text
//! ~/.claude/projects/<project-slug>/<uuid>.jsonl
//! ```
//!
//! The project-slug is derived from the cwd (every non-alphanumeric character becomes `-`), so
//! sessions are already partitioned by project — which makes "list the sessions belonging to one
//! repo" cheap (a single readdir), in contrast with Codex.
//!
//! One message per line; the load-bearing fields are `type` (user/assistant), `message.content`
//! (a string or an array of blocks), `uuid` / `parentUuid` (the message chain), `sessionId` and
//! `cwd`.
//!
//! # Resume
//!
//! ```bash
//! claude --resume <uuid>
//! ```
//!
//! It scans the project directory matching on id; there is no index to maintain. **The id must
//! be a UUID**, so install writes a freshly generated one.

use super::{
    Adapter, Capability, Event, EventKind, Installed, Next, OpenCall, Session, SessionRef,
};
use crate::Result;
use anyhow::Context;
use std::path::{Path, PathBuf};

pub struct ClaudeCode;

fn projects_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("$HOME is not set")?;
    Ok(PathBuf::from(home).join(".claude").join("projects"))
}

/// Map a cwd onto Claude Code's project directory name.
///
/// Must match Claude Code's own algorithm, or it never finds the session installed into it.
pub fn slug_for(cwd: &Path) -> String {
    crate::domain::store::slug_for(cwd)
}

impl Adapter for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn cli(&self) -> &'static str {
        "claude"
    }

    fn capability(&self) -> Capability {
        Capability::Resumable
    }

    fn format(&self) -> &'static str {
        // The Code tab of Claude Desktop writes this same jsonl (§4.2) — they are one
        // format family, and installing across them is a byte rewrite.
        "claude-code"
    }

    fn sessions_for(&self, repo: &Path) -> Result<Vec<SessionRef>> {
        let dir = projects_dir()?.join(slug_for(repo));
        if !dir.exists() {
            // This project has never run under Claude Code — a normal state, not an error.
            return Ok(vec![]);
        }
        collect_dir(&dir, Some(repo.to_string_lossy().to_string()))
    }

    fn all_sessions(&self) -> Result<Vec<SessionRef>> {
        let root = projects_dir()?;
        if !root.exists() {
            return Ok(vec![]);
        }
        let mut out = vec![];
        for e in std::fs::read_dir(&root)? {
            let Ok(e) = e else { continue };
            if !e.path().is_dir() {
                continue;
            }
            // The directory name is a slug and the original cwd cannot be recovered from it
            // (every non-alphanumeric character became `-`), so cwd stays empty here — a caller
            // that needs it reads the transcript.
            out.extend(collect_dir(&e.path(), None)?);
        }
        Ok(out)
    }

    /// Reverse lookup: with a cwd, go straight there (0.14 ms); without one, or on a miss, glob
    /// one level (2.2 ms).
    ///
    /// Going straight there works because the project directory name is the slug of the cwd (all
    /// ten real samples agree). That is Claude Code's internal convention, not something we can
    /// guarantee, so the fallback has to exist.
    fn resolve(&self, session_id: &str, cwd: Option<&Path>) -> Option<PathBuf> {
        let root = projects_dir().ok()?;

        if let Some(c) = cwd {
            let direct = root.join(slug_for(c)).join(format!("{session_id}.jsonl"));
            if direct.is_file() {
                return Some(direct);
            }
        }

        // One level is enough: a project directory holds `<id>.jsonl` directly, no recursion.
        for e in std::fs::read_dir(&root).ok()? {
            let Ok(e) = e else { continue };
            let p = e.path().join(format!("{session_id}.jsonl"));
            if p.is_file() {
                return Some(p);
            }
        }
        None
    }

    fn parse(&self, text: &str) -> Result<Session> {
        let mut id = String::new();
        let mut cwd = None;
        let mut events = vec![];

        // Line numbers start at 0 and go into every event ([`Event::line`]): the web transcript
        // uses them to go back to the source for tool arguments and thinking bodies. Blank and
        // corrupt lines take a number too, so the coordinate addresses a line of the file
        // directly.
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // A single corrupt line is skipped: a transcript can be truncated (the process was
            // killed), and a partial session is still worth resuming. Failing the whole parse
            // loses more.
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };

            if id.is_empty()
                && let Some(s) = v.get("sessionId").and_then(|x| x.as_str())
            {
                id = s.to_string();
            }
            if cwd.is_none()
                && let Some(c) = v
                    .get("cwd")
                    .and_then(|x| x.as_str())
                    .filter(|c| !c.is_empty())
            {
                cwd = Some(c.to_string());
            }

            let ts = v
                .get("timestamp")
                .and_then(|x| x.as_str())
                .map(String::from);
            let content = v.get("message").and_then(|m| m.get("content"));

            match v.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                "user" => {
                    // Tool output also arrives as `type: "user"` (all 2211 observed blocks do),
                    // but nobody typed it. Split it off first, or the `extract_text` below —
                    // which only knows `text` blocks — yields no event at all for the record:
                    // unsearchable, and not counted as dropped either. See
                    // `EventKind::ToolResult`.
                    if let Some(outputs) = extract_tool_results(content) {
                        for t in outputs {
                            events.push(
                                Event::text(EventKind::ToolResult, t, ts.clone()).at_line(lineno),
                            );
                        }
                        continue;
                    }
                    if let Some(t) = extract_text(content) {
                        // A compact summary is a **synthesized** user record: `type` and
                        // `message.role` are both `"user"`, but nobody typed it. The only
                        // reliable test is the top-level `isCompactSummary: true` — the body
                        // format is not stable (of two observed samples one uses `Summary:`,
                        // the other `<summary>`), so it must never be decided from the text.
                        let kind = if is_compact_summary(&v) {
                            EventKind::CompactSummary
                        } else if is_system_generated(&v, &t) || is_synthetic_user_text(&t) {
                            // Runtime-injected, not typed. Recorded as Other so it counts as
                            // dropped without polluting UserPrompt — turn splitting keys off
                            // UserPrompt, and a misjudgement cuts a pile of one-event fake
                            // turns.
                            EventKind::Other
                        } else {
                            EventKind::UserPrompt
                        };
                        events.push(Event::text(kind, t, ts).at_line(lineno));
                    }
                }
                "assistant" => {
                    // content is an array of blocks and can carry text and tool calls at once;
                    // both are collected — a tool call is the main evidence of what the agent
                    // did.
                    if let Some(arr) = content.and_then(|c| c.as_array()) {
                        for b in arr {
                            match b.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                                "text" => {
                                    if let Some(t) = b
                                        .get("text")
                                        .and_then(|x| x.as_str())
                                        .filter(|t| !t.trim().is_empty())
                                    {
                                        events.push(
                                            Event::text(EventKind::AssistantReply, t, ts.clone())
                                                .at_line(lineno),
                                        );
                                    }
                                }
                                "tool_use" => {
                                    let name = b
                                        .get("name")
                                        .and_then(|x| x.as_str())
                                        .unwrap_or("tool")
                                        .to_string();
                                    let paths = extract_paths(b.get("input"));
                                    events.push(Event {
                                        // A call carrying a file path counts as an edit —
                                        // this is the signal for what the session changed.
                                        kind: if paths.is_empty() {
                                            EventKind::ToolUse
                                        } else {
                                            EventKind::FileEdit
                                        },
                                        text: Some(name.clone()),
                                        timestamp: ts.clone(),
                                        paths,
                                        tool: Some(name),
                                        line: Some(lineno),
                                    });
                                }
                                // thinking blocks are vendor-specific; the IR does not model
                                // them.
                                "thinking" | "redacted_thinking" => {
                                    events.push(other_event(ts.clone()).at_line(lineno));
                                }
                                // An unknown assistant block type (a tool-interaction block
                                // added later, say) counts as dropped too — dropping it
                                // silently breaks the rule set at the top of adapter/mod.rs
                                // (loss must be explicit and reportable).
                                _ => {
                                    events.push(other_event(ts.clone()).at_line(lineno));
                                }
                            }
                        }
                    } else if let Some(t) = extract_text(content) {
                        events.push(Event::text(EventKind::AssistantReply, t, ts).at_line(lineno));
                    }
                }
                // ── Cowork / desktop-app record types ──
                //
                // A Claude Desktop Cowork session is the same jsonl plus three more `type`s
                // (observed; see docs/mechanism-probing/desktop-apps.md §3.2):
                //
                //   attachment        a runtime-injected delta of the tool / skill / agent list
                //   last-prompt       a **copy** of the user prompt (one sentence appears 4 times)
                //   queue-operation   the lifecycle of queued user input (see below)
                //
                // The first two must map to Other rather than fall into the catch-all below —
                // the catch-all gives Other as well, but listing them says **why** they are not
                // UserPrompt: taken as prompts, one sentence is counted 6 times (1 real + 4
                // last-prompt + 1 enqueue), and gist and turn splitting break together.
                "attachment" | "last-prompt" => {
                    events.push(other_event(ts).at_line(lineno));
                }
                // A queue record has two fates, by `operation`:
                //
                //   enqueue → dequeue   the sentence becomes the next turn's prompt and shows up
                //                       again as a normal `user` record — this one is only a
                //                       copy, recorded as Other.
                //   enqueue → remove    `reason: absorbed_mid_turn`: what the user kept saying
                //                       while the agent was still running tools, absorbed into
                //                       the **current turn**. It never becomes a `user` record,
                //                       and this remove holds the only verbatim copy (the one
                //                       inside tool_result is the runtime's paraphrase).
                //
                // The latter is a human speaking; dropping it leaves the resumed agent unaware
                // that the user changed the request mid-way. It does not open a new turn (see
                // `EventKind::UserInterjection`).
                "queue-operation" => {
                    events.push(
                        match absorbed_mid_turn_text(&v) {
                            Some(t) => Event::text(EventKind::UserInterjection, t, ts),
                            None => other_event(ts),
                        }
                        .at_line(lineno),
                    );
                }
                // Every other unknown type is Other as well: dropping silently makes "how much
                // the conversion loses" unreportable.
                _ => {
                    events.push(other_event(ts).at_line(lineno));
                }
            }
        }

        Ok(Session {
            id,
            runtime: "claude-code".into(),
            cwd,
            events,
        })
    }

    fn render(&self, session: &Session, new_id: &str, cwd: &Path) -> Result<String> {
        self.render_with(session, new_id, cwd, &super::ToolDetails::default())
    }

    fn render_with(
        &self,
        session: &Session,
        new_id: &str,
        cwd: &Path,
        details: &super::ToolDetails,
    ) -> Result<String> {
        let mut out = String::new();
        let cwd_s = cwd.to_string_lossy().to_string();
        // Claude Code chains records by parentUuid, so each record remembers the uuid before
        // it.
        let mut parent: Option<String> = None;
        let mut minted = 0usize;

        for (ei, e) in session.events.iter().enumerate() {
            // A receipt-shaped edit (marked by the source extractor, see
            // ToolDetails::receipts) falls through to the change-signal text below instead of
            // minting a second call.
            if e.kind == EventKind::ToolUse
                || (e.kind == EventKind::FileEdit && !details.is_receipt(ei))
            {
                // A tool call (an edit is a call too) goes out as a real `tool_use` block with
                // its `tool_result` beside it — emit it as plain text pretending to be a reply
                // and the shape the model sees no longer matches the source side. Arguments and
                // output come back from the source transcript through enrich; when they cannot,
                // the arguments are just the file path (the paths of an edit event) and the
                // output is a placeholder. A failed call carries `is_error`. The pairing id is
                // minted and only has to be unique within this artifact.
                minted += 1;
                let tid = format!("toolu_agit_{minted}");
                let name = e
                    .tool
                    .clone()
                    .or_else(|| e.text.clone())
                    .unwrap_or_else(|| "tool".into());
                let d = details.get(ei);
                let input = d
                    .and_then(|d| d.input.clone())
                    .map(|v| {
                        if v.is_object() {
                            v
                        } else {
                            serde_json::json!({ "arguments": v })
                        }
                    })
                    .unwrap_or_else(|| match e.paths.first() {
                        Some(p) => serde_json::json!({ "file_path": p }),
                        None => serde_json::json!({}),
                    });
                let is_error = d.is_some_and(|d| d.error);
                let output = d
                    .and_then(|d| d.output.clone())
                    .unwrap_or_else(|| super::codex::CROSS_RUNTIME_OUTPUT_PLACEHOLDER.into());
                let ts = e
                    .timestamp
                    .clone()
                    .unwrap_or_else(|| "2026-01-01T00:00:00.000Z".to_string());
                let use_uuid = uuid::Uuid::now_v7().to_string();
                out.push_str(&serde_json::to_string(&serde_json::json!({
                    "cwd": cwd_s,
                    "gitBranch": "",
                    "isSidechain": false,
                    "message": { "content": [{
                        "type": "tool_use", "id": tid, "name": name, "input": input
                    }], "role": "assistant" },
                    "parentUuid": parent,
                    "sessionId": new_id,
                    "timestamp": ts,
                    "type": "assistant",
                    "userType": "external",
                    "uuid": use_uuid,
                    "version": "2.1.207",
                }))?);
                out.push('\n');
                let res_uuid = uuid::Uuid::now_v7().to_string();
                out.push_str(&serde_json::to_string(&serde_json::json!({
                    "cwd": cwd_s,
                    "gitBranch": "",
                    "isSidechain": false,
                    "message": { "content": [{
                        "type": "tool_result", "tool_use_id": tid, "content": output,
                        "is_error": is_error
                    }], "role": "user" },
                    "parentUuid": use_uuid,
                    "sessionId": new_id,
                    "timestamp": ts,
                    "type": "user",
                    "userType": "external",
                    "uuid": res_uuid,
                    "version": "2.1.207",
                }))?);
                out.push('\n');
                parent = Some(res_uuid);
                continue;
            }
            let (ty, role) = match e.kind {
                // An interjection goes out as an ordinary user message: the resumed agent
                // needs to know what the user said mid-turn, and the target runtime has no
                // "absorbed into the current turn" form to restore it into.
                EventKind::UserPrompt | EventKind::UserInterjection => ("user", "user"),
                // A FileEdit that reaches here is a receipt: its body (`edited N files`, say)
                // is its change signal.
                EventKind::AssistantReply | EventKind::FileEdit => ("assistant", "assistant"),
                // Already emitted as a pair (tool_use + tool_result) at the top of the loop;
                // unreachable here.
                EventKind::ToolUse => continue,
                // A compact boundary **is emitted** (the render policy flipped, 2026-08).
                //
                // The reason for the flip: the filtering decision — should this stretch of
                // context appear in the resume artifact — sits with each branch's resume VIEW
                // (session/VIEW, built at commit), which decides by branch intent, leaving
                // render nothing but faithful reconstruction. And a compact summary is exactly
                // the **legitimate opening context of a compacted session once it resumes**:
                // when Claude Code resumes on its own, it is the first message sent to the
                // model. If render does not emit it, the restored session has no beginning.
                //
                // Shape: a **plain** user message, deliberately without the
                // `isCompactSummary` marker — that marker is a trace of the **source** runtime's
                // context management; to the target runtime this is a stretch of context, not a
                // boundary signal.
                EventKind::CompactSummary => ("user", "user"),
                // A filter boundary is emitted for the same reason, but its body is not
                // conversation content (it is the source runtime's filtering marker, kept
                // verbatim in the canonical layer), so it carries an explicit label letting the
                // resumed agent see a boundary note rather than something the user said.
                EventKind::CompactFiltered => ("user", "user"),
                // A turn-end marker is the source runtime's internal signal, not conversation
                // content.
                //
                // A ToolResult event is still never replayed on its own: enrich has already
                // paired its body to the source-side call id and it goes out with the
                // `tool_use` above; where it cannot be recovered, that pairing carries a
                // placeholder. Emitting it separately repeats the same output twice, or
                // disguises ownerless output as something the user pasted in by hand.
                EventKind::ToolResult | EventKind::TurnEnd | EventKind::Other => continue,
            };
            let text: String = if e.kind == EventKind::CompactFiltered {
                format!(
                    "(this context was compact-filtered by the source runtime: {})",
                    e.text
                        .as_deref()
                        .filter(|t| !t.trim().is_empty())
                        .unwrap_or("window / kept-count unknown")
                )
            } else {
                let Some(t) = e.text.as_deref().filter(|t| !t.trim().is_empty()) else {
                    continue;
                };
                t.to_string()
            };

            let uuid = uuid::Uuid::now_v7().to_string();
            // A missing timestamp falls back to a fixed value rather than now(): render must
            // be a pure function, or repeated conversion shows git a file that changes forever
            // (delta compression stops working).
            let ts = e
                .timestamp
                .clone()
                .unwrap_or_else(|| "2026-01-01T00:00:00.000Z".to_string());

            // Claude Code only accepts an assistant `content` in array form.
            let content = if role == "assistant" {
                serde_json::json!([{ "type": "text", "text": text }])
            } else {
                serde_json::Value::String(text)
            };

            out.push_str(&serde_json::to_string(&serde_json::json!({
                "cwd": cwd_s,
                "gitBranch": "",
                "isSidechain": false,
                "message": { "content": content, "role": role },
                "parentUuid": parent,
                "sessionId": new_id,
                "timestamp": ts,
                "type": ty,
                "userType": "external",
                "uuid": uuid,
                "version": "2.1.207",
            }))?);
            out.push('\n');
            parent = Some(uuid);
        }
        Ok(out)
    }

    fn open_tool_calls(&self, text: &str) -> Vec<OpenCall> {
        // `tool_use.id` ↔ `tool_result.tool_use_id`; the call sits in the block array of an
        // assistant record, the result in a later user record.
        let mut open: Vec<OpenCall> = Vec::new();
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            let Some(blocks) = v
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            else {
                continue;
            };
            for b in blocks {
                match b.get("type").and_then(|x| x.as_str()) {
                    Some("tool_use") => {
                        if let Some(id) = b.get("id").and_then(|x| x.as_str()) {
                            open.push(OpenCall {
                                line: lineno,
                                call_id: id.to_string(),
                                record: "tool_use".into(),
                                name: b
                                    .get("name")
                                    .and_then(|x| x.as_str())
                                    .unwrap_or("tool")
                                    .to_string(),
                            });
                        }
                    }
                    Some("tool_result") => {
                        if let Some(id) = b.get("tool_use_id").and_then(|x| x.as_str()) {
                            open.retain(|c| c.call_id != id);
                        }
                    }
                    _ => {}
                }
            }
        }
        open
    }

    fn mint_id(&self) -> String {
        // Claude Code requires the UUID form.
        uuid::Uuid::now_v7().to_string()
    }

    fn install(&self, content: &str, new_id: &str, cwd: &Path) -> Result<Installed> {
        let dir = projects_dir()?.join(slug_for(cwd));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let path = dir.join(format!("{new_id}.jsonl"));
        std::fs::write(&path, content)
            .with_context(|| format!("cannot write {}", path.display()))?;
        Ok(Installed {
            next: Next::Resume(format!(
                "(cd {} && claude --resume {new_id})",
                cwd.display()
            )),
            path,
        })
    }
}

/// Whether this `type=user` message was injected by the runtime rather than typed by a human.
///
/// # Why it is needed
///
/// Claude Code stuffs a great deal into `type=user` as well. Across 259 local transcripts and 549
/// plain-text user messages, **only 241 were typed by a human** (44%):
///
/// | Count | What it is |
/// |---|---|
/// | 247 | `<task-notification>` background-task completion notices |
/// | 241 | real user input |
/// | 21 | `<command-name>` slash commands (`/model` / `/effort`) |
/// | 19 | `<local-command-stdout>` output of a slash command |
/// | 11 | `<local-command-caveat>` note on a command-generated message |
/// | 8 | `[Your previous response had no visible output…]` system follow-up |
///
/// Plus, in the block-array form, `[Request interrupted by user]` (38) and
/// `[Request interrupted by user for tool use]` (7).
///
/// # Consequence
///
/// Without the filter, [`crate::domain::turn`]'s turn splitting takes every injection for the
/// start of a turn and cuts a pile of fake turns holding one event each (a 3 MB session was
/// observed cutting into 10 turns, 4 of them `<task-notification>` and
/// `[Request interrupted by user]`). The hash chain is where version IDs come from, so a fake
/// turn points a version ID at a meaningless place.
///
/// # The test
///
/// Same as the Codex side: match on **known prefixes**, not on "anything starting with `<`" — a
/// user can perfectly well paste an HTML/XML fragment. Better to miss a new injection shape than
/// to judge real input synthetic.
///
/// `[Image: original …]` is **not** an injection: it is the note the runtime puts in front of a
/// real message when the user pastes an image, and what follows is what the human said.
///
/// # Second census (684 real transcripts, 2526 `type=user` records with a body)
///
/// Without the nine shapes below the table takes **110 records** for human questions. The
/// consequence is not only turn splitting: `search::outcome`'s "the same question was asked again
/// N times" signal counts a repeatedly injected template body as a re-ask, so a session doing
/// ordinary work is marked `failed`. Of 66 sessions judged to have re-asked, **11 (17%)** come
/// from this.
///
/// Every entry added is verified to be **runtime words from beginning to end** (the test is the
/// one behind the Codex-side table):
///
/// * `<command-message>` (48) one of the three parts of a slash command. **The table already
///   holds `<command-name>` and still does not catch this** — the observed shape is
///   `<command-message>init is analyzing…</command-message>\n<command-name>/init</command-name>`,
///   where `<command-name>` sits on the second line while the test here is `starts_with`
/// * `<teammate-message` (24) a multi-agent collaboration message. **Deliberately without the
///   closing `>`**: it carries `teammate_id` / `color` / `summary` attributes
/// * `Caveat: The messages below were generated by the user while ` (23) the **bare form** of
///   `<local-command-caveat>`, with no tag around it, all 23 word for word identical
/// * `<user-prompt-submit-hook>` (7) output of the submit hook
/// * `<session-start-hook>` (4) the MCP tool list injected at session start
/// * `<bash-input>` / `<bash-stdout>` / `<bash-notification>` (1 each)
///   the `!`-prefixed bash mode; each of the three closes on its own
/// * `Your tool call was malformed and could not be parsed` (1) the runtime's retry prompt
///
/// # The five deliberately left out
///
/// The first two are verified to **carry human words on the tail**, and adding them makes those
/// words disappear — the same reason the Codex side excludes `<ide_opened_file>`, a conclusion
/// this corpus reproduces independently:
///
/// * `<ide_opened_file>` (3) — observed as `…</ide_opened_file>\nNow I would like to add …`
///   and `…</ide_opened_file>\nclaude plugin install figma@…`
/// * `Base directory for this skill:` (5) — a skill body, but its **tail is
///   `ARGUMENTS: <user input>`**. One observed record ends `ARGUMENTS: xoa het`
///
/// The other three are things a human may well type, and stay out under "better to miss an
/// injection than to misjudge real input":
///
/// * `Please analyze this codebase and create a CLAUDE.md file` (16) the expansion of `/init`
/// * `## [meta]` + ```` ```json ```` (16) the template body of a slash command
/// * `Continue from where you left off.` (6) — a human types this sentence
///
/// # Why not `isMeta`
///
/// That field looks more fundamental than a prefix table: of 120 records with `isMeta = true`, 88
/// are injections the prefix table cannot recognize (the `/init` expansion and the template
/// bodies among them). **But it kills real input**: the 4 `[Image: original 2160x742…]` records
/// carry `isMeta = true` too, and a record of that shape is followed by what the human said.
/// Loosening the test loses real input, so the prefix table stays.
fn is_synthetic_user_text(text: &str) -> bool {
    const INJECTED: &[&str] = &[
        "<task-notification>",
        "<command-name>",
        "<command-message>",
        "<local-command-stdout>",
        "<local-command-caveat>",
        "<system-reminder>",
        "<user-prompt-submit-hook>",
        "<session-start-hook>",
        "<bash-input>",
        "<bash-stdout>",
        "<bash-notification>",
        // Attributes follow, so there is no closing `>`.
        "<teammate-message",
        "[Your previous response had no visible output",
        "[Request interrupted by user]",
        "[Request interrupted by user for tool use]",
        // The activation notice of a Stop hook (17 observed). It reads exactly like a human —
        // the body embeds the condition the user set — so only this fixed opening identifies it.
        "A session-scoped Stop hook is now active",
        // The bare form of `<local-command-caveat>`, all 23 observed word for word identical.
        "Caveat: The messages below were generated by the user while ",
        "Your tool call was malformed and could not be parsed",
        // The runtime's receipt for a mistyped slash command (4 observed across 3 sessions).
        // The whole record is this sentence plus the word the user got wrong, with no human
        // words after it — the same class as `<local-command-stdout>`, only without a tag around
        // it.
        //
        // It enters the table not because the count runs high but because **success or failure
        // gets the wrong label**: someone who mistypes a slash command usually tries again right
        // away, so two word-for-word identical "prompts" become one "re-ask". One observed
        // session is judged `Failed` this way.
        "Unknown slash command:",
    ];
    let s = text.trim_start();
    INJECTED.iter().any(|p| s.starts_with(p))
}

/// Whether this `type=user` record was **generated by the runtime itself** rather than sent in by
/// a human.
///
/// # Why a prefix cannot decide this one
///
/// Claude Code has a class of injection that reads exactly like a human. The observed one is
/// `Goal set: build the new evaluation method plus v1 / v1_lite. Update the repo, the paper and
/// the homepage.` — a turn the runtime continues on its own after `/goal`, whose sentence is the
/// goal text the user set earlier. Any prefix rule that recognizes it (`Goal set:`) also swallows
/// a human-typed "Goal set: fix CI".
///
/// # The test: `isMeta` / `promptSource`
///
/// A user-shaped record injected by a tool or a Skill carries `isMeta: true`. A Skill body, for
/// instance, displays as `Base directory for this skill: …`, but nothing needs to lean on that
/// English sentence: the structured field already says it is a runtime meta message.
///
/// For the remaining user records, Claude Code records the source itself. Across 34 local
/// transcripts and 216 user records with a body:
///
/// | `promptSource` | Count | What it is |
/// |---|---|---|
/// | `typed` | 68 | typed into the terminal by a human |
/// | `sdk` | 70 | sent in through the SDK (still a human's intent) |
/// | `queued` | 13 | queued by a human, waiting to go out |
/// | `system` | 1 | runtime-generated (that record also has `origin.kind = auto-continuation`) |
/// | missing | 64 | older records, plus `<command-name>`-style prefix-table injections |
///
/// `isMeta` accepts only the boolean `true` and `promptSource` only `"system"`; neither accepts
/// "missing" — the overwhelming majority of the records missing the field are real human input,
/// and treating missing as injection erases all of them. `origin.kind` is not accepted either:
/// it appears on a single record, too little to be a test.
///
/// # The one exception to `isMeta`: the pasted-image note
///
/// The 120 `isMeta = true` records in the census are not one kind of thing (see "Why not
/// `isMeta`" in [`is_synthetic_user_text`]): 4 of them record a **user pasting an image**, where
/// the runtime puts `[Image: original …]` **in front of** the sentence the human actually said
/// and the whole record still carries `isMeta`. The field says "this record contains words the
/// runtime added", not "the whole record is runtime-generated". So `isMeta` is not accepted when
/// the body opens that way — otherwise the instruction the user asked alongside the image
/// disappears from `prompts`, the turn boundaries lose one, and since `render` skips `Other`, the
/// restored session does not hold it at all. `promptSource == "system"` is untouched by this
/// exception: that is a turn the runtime started **itself**, nothing to do with pasting an
/// image.
fn is_system_generated(v: &serde_json::Value, text: &str) -> bool {
    let meta_injection =
        v.get("isMeta").and_then(|x| x.as_bool()) == Some(true) && !carries_image_note(text);
    meta_injection || v.get("promptSource").and_then(|x| x.as_str()) == Some("system")
}

/// Whether the body opens with a pasted-image note.
///
/// The note looks like `[Image: original 2100x1308, displayed at 2000x1246.]` or
/// `[Image: original 2160x742…]`, and what follows is always what the human actually said —
/// which is why [`is_synthetic_user_text`]'s prefix table deliberately leaves it out too. Only
/// the opening is inspected, precisely because what must be kept is **what comes after**.
fn carries_image_note(text: &str) -> bool {
    text.trim_start().starts_with("[Image: ")
}

/// The user's own words absorbed into the current turn by a `queue-operation` record, if any.
///
/// Only `operation == "remove"` with `reason == "absorbed_mid_turn"` counts: an `enqueue`'s
/// content shows up again as a normal `user` record, so accepting it double-counts, and other
/// `remove` reasons (a user retracting, say) never entered the conversation. The content still
/// has to pass [`is_synthetic_user_text`] — background-task notices go through this queue too,
/// in a shape identical to human typing.
fn absorbed_mid_turn_text(v: &serde_json::Value) -> Option<String> {
    let op = v.get("operation").and_then(|x| x.as_str())?;
    let reason = v.get("reason").and_then(|x| x.as_str());
    if op != "remove" || reason != Some("absorbed_mid_turn") {
        return None;
    }
    let text = v.get("content").and_then(|x| x.as_str())?;
    if text.trim().is_empty() || is_synthetic_user_text(text) {
        return None;
    }
    Some(text.to_string())
}

fn other_event(ts: Option<String>) -> Event {
    Event {
        kind: EventKind::Other,
        text: None,
        timestamp: ts,
        paths: vec![],
        tool: None,
        line: None,
    }
}

fn collect_dir(dir: &Path, cwd: Option<String>) -> Result<Vec<SessionRef>> {
    let mut out = vec![];
    for e in std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let Ok(e) = e else { continue };
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        out.push(SessionRef {
            id: stem.to_string(),
            path: p,
            runtime: "claude-code",
            cwd: cwd.clone(),
            mtime: e
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
            gist: None,
        });
    }
    Ok(out)
}

/// Whether this record is a compact summary.
///
/// The test looks only at the top-level `isCompactSummary`. In the observed samples (two files
/// under `~/.claude/projects/…/subagents/`) the record has this shape:
///
/// ```json
/// {
///   "type": "user",
///   "message": { "role": "user", "content": "This session is being continued…" },
///   "isCompactSummary": true,
///   "parentUuid": null
/// }
/// ```
///
/// `parentUuid: null` is a signal too (the message chain resets here), but it cannot be the
/// test — the **first** message of a session is `parentUuid: null` as well.
fn is_compact_summary(v: &serde_json::Value) -> bool {
    v.get("isCompactSummary")
        .and_then(|x| x.as_bool())
        .unwrap_or(false)
}

/// Take the plain text out of `message.content`. content is a string or an array of blocks.
fn extract_text(content: Option<&serde_json::Value>) -> Option<String> {
    let c = content?;
    if let Some(s) = c.as_str() {
        return Some(s.to_string()).filter(|s| !s.trim().is_empty());
    }
    let arr = c.as_array()?;
    let parts: Vec<&str> = arr
        .iter()
        .filter(|b| b.get("type").and_then(|x| x.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|x| x.as_str()))
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// Take the body of the tool output out of a `type: "user"` record.
///
/// `None` means "this record is not tool output" — the caller decides from that whether to treat
/// it as an ordinary user message. `Some(vec![])` is possible (output blocks exist but every body
/// is an image or an empty string); that case **still counts as tool output** and must not fall
/// back to the user-message path.
///
/// # Shape (observed across 21 real sessions, 2211 blocks)
///
/// ```text
/// content:  [{"type":"tool_result","tool_use_id":"…","content":<one of the three below>}]
///           two key sets: 1507 with is_error, 704 without
///
/// inner content:  string              2200  ← the vast majority
///                 [{"type":"text"}]      7
///                 [{"type":"image"}]     4  ← no text to take, skipped
/// ```
///
/// `is_error` is **ignored**: an error output is still evidence of what the command printed, and
/// a failing output is often exactly what someone comes searching for
/// (`Permission denied (publickey)`). Judging success or failure is `outcome`'s job, not this
/// one's.
fn extract_tool_results(content: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let arr = content?.as_array()?;
    let mut saw_tool_result = false;
    let mut out = Vec::new();
    for b in arr {
        if b.get("type").and_then(|x| x.as_str()) != Some("tool_result") {
            continue;
        }
        saw_tool_result = true;
        let Some(inner) = b.get("content") else {
            continue;
        };
        if let Some(s) = inner.as_str() {
            if !s.trim().is_empty() {
                out.push(s.to_string());
            }
            continue;
        }
        // The block-array shape: only text is collected; an image has no text to take.
        if let Some(blocks) = inner.as_array() {
            let parts: Vec<&str> = blocks
                .iter()
                .filter(|ib| ib.get("type").and_then(|x| x.as_str()) == Some("text"))
                .filter_map(|ib| ib.get("text").and_then(|x| x.as_str()))
                .filter(|t| !t.trim().is_empty())
                .collect();
            if !parts.is_empty() {
                out.push(parts.join("\n"));
            }
        }
    }
    if saw_tool_result { Some(out) } else { None }
}

/// Take file paths out of a tool call's input.
///
/// Only a few agreed key names are recognized. The heuristic is deliberate: the point is to tell
/// which files a call touched, and missing some only makes the count low, never wrong.
fn extract_paths(input: Option<&serde_json::Value>) -> Vec<String> {
    let Some(v) = input else { return vec![] };
    ["file_path", "path", "notebook_path"]
        .iter()
        .filter_map(|k| v.get(*k).and_then(|x| x.as_str()))
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_content_shapes() {
        let text = r#"{"type":"user","message":{"role":"user","content":"hi"},"sessionId":"s1","cwd":"/repo","timestamp":"2026-01-01T00:00:00Z","uuid":"u1"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"sure"},{"type":"tool_use","name":"Edit","input":{"file_path":"/repo/a.rs"}}]},"sessionId":"s1","uuid":"u2","parentUuid":"u1"}"#;
        let s = ClaudeCode.parse(text).unwrap();
        assert_eq!(s.id, "s1");
        assert_eq!(s.cwd.as_deref(), Some("/repo"));
        let c = s.counts();
        assert_eq!(c.prompts, 1);
        assert_eq!(c.replies, 1);
        assert_eq!(c.edits, 1, "a tool_use with file_path is an edit");
    }

    /// **Tool output must not leave a record eventless.**
    ///
    /// `tool_result` arrives as `type: "user"` and `extract_text` only collects `text` blocks, so
    /// `tool_use.id` pairs with `tool_result.tool_use_id`; a call whose result has not arrived is
    /// open.
    #[test]
    fn open_tool_calls_pair_tool_use_with_tool_result() {
        let call = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"running it"},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"agit commit"}}]},"sessionId":"s"}"#;
        let result = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"done"}]},"sessionId":"s"}"#;
        let open = ClaudeCode.open_tool_calls(&format!("{call}\n"));
        assert_eq!(
            open,
            vec![OpenCall {
                line: 0,
                call_id: "toolu_1".into(),
                record: "tool_use".into(),
                name: "Bash".into(),
            }]
        );
        assert!(
            ClaudeCode
                .open_tool_calls(&format!("{call}\n{result}\n"))
                .is_empty()
        );
    }

    /// The record is neither `UserPrompt` nor `Other` — it does not exist at all. Across 13123
    /// lines of real corpus there are 2211 such blocks, none of which reach the IR, and 40% of the
    /// distinct config-assignment shapes live only inside them.
    #[test]
    fn tool_output_becomes_an_event_instead_of_vanishing() {
        let text = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"lr: 2.000e-03\nmin_lr: 1.25e-5"}]},"sessionId":"s1"}"#;
        let s = ClaudeCode.parse(text).unwrap();
        assert_eq!(s.events.len(), 1);
        assert_eq!(s.events[0].kind, EventKind::ToolResult);
        assert!(s.events[0].text.as_deref().unwrap().contains("2.000e-03"));
        let c = s.counts();
        assert_eq!(c.outputs, 1);
        assert_eq!(c.prompts, 0, "tool output is not something a human typed");
        assert_eq!(c.dropped, 0, "it is in the IR, not dropped");
    }

    /// **`render` must never emit tool output as a plain user text message.**
    ///
    /// That makes the resumed agent believe the user pasted a whole block of command output in by
    /// hand — passing off what the machine printed as what the human said. Its correct place is
    /// the paired `tool_use_id`: the real body when enrich recovers it, a placeholder when it does
    /// not — both are paired `tool_result`s, not something the user said.
    #[test]
    fn render_does_not_replay_tool_output_as_a_user_message() {
        let s = Session {
            id: "s".into(),
            runtime: "claude-code".into(),
            cwd: None,
            events: vec![
                Event::text(EventKind::UserPrompt, "show me lr", None),
                Event::text(EventKind::ToolUse, "Bash", None),
                Event::text(EventKind::ToolResult, "lr: 2.000e-03", None),
            ],
        };
        let out = ClaudeCode.render(&s, "new", Path::new("/repo")).unwrap();
        assert!(
            !out.contains("2.000e-03"),
            "without enrich the source output must not appear in the resume artifact:\n{out}"
        );
        // The human words and the call itself must still be there, the call carrying its paired
        // placeholder output.
        assert!(out.contains("show me lr"));
        assert!(out.contains(crate::adapter::codex::CROSS_RUNTIME_OUTPUT_PLACEHOLDER));
        assert!(
            ClaudeCode.open_tool_calls(&out).is_empty(),
            "every call must be closed"
        );
        let back = ClaudeCode.parse(&out).unwrap();
        assert_eq!(back.counts().prompts, 1, "no fake user prompt is added");
        assert_eq!(back.counts().outputs, 1, "placeholder output is paired");
        assert_eq!(back.counts().tools, 1, "the call is a real tool_use block");
    }

    /// With details from enrich the call carries real arguments and the pair carries real
    /// output — but the output still lives only in `tool_result`, never becomes something the
    /// user said.
    #[test]
    fn render_with_details_carries_arguments_and_paired_output() {
        let s = Session {
            id: "s".into(),
            runtime: "claude-code".into(),
            cwd: None,
            events: vec![
                Event::text(EventKind::UserPrompt, "show me lr", None),
                Event::text(EventKind::ToolUse, "Bash", None),
            ],
        };
        let mut details = crate::adapter::ToolDetails::default();
        details.insert(
            1,
            crate::adapter::ToolDetail {
                input: Some(serde_json::json!({"command": "cat lr.txt"})),
                output: Some("lr: 2.000e-03".into()),
                error: false,
            },
        );
        let out = ClaudeCode
            .render_with(&s, "new", Path::new("/repo"), &details)
            .unwrap();
        let lines: Vec<serde_json::Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let call = lines
            .iter()
            .find(|v| v["message"]["content"][0]["type"] == "tool_use")
            .expect("a tool_use block is emitted");
        assert_eq!(
            call["message"]["content"][0]["input"]["command"],
            "cat lr.txt"
        );
        let result = lines
            .iter()
            .find(|v| v["message"]["content"][0]["type"] == "tool_result")
            .expect("a tool_result block is emitted");
        assert_eq!(
            result["message"]["content"][0]["tool_use_id"], call["message"]["content"][0]["id"],
            "the output pairs to this call"
        );
        assert_eq!(result["message"]["content"][0]["content"], "lr: 2.000e-03");
        assert_eq!(
            result["parentUuid"], call["uuid"],
            "the parentUuid chain runs through the whole pair"
        );
        let back = ClaudeCode.parse(&out).unwrap();
        assert_eq!(back.counts().prompts, 1, "output is not a user message");
    }

    /// A failing output **is collected too**.
    ///
    /// `is_error: true` does not change whether it is evidence — `Permission denied (publickey)`
    /// is exactly the kind of string someone comes searching for. Judging success or failure is
    /// `outcome`'s job.
    #[test]
    fn a_failing_tool_result_is_still_evidence() {
        let text = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"Exit code 255\nPermission denied (publickey)."}]},"sessionId":"s1"}"#;
        let s = ClaudeCode.parse(text).unwrap();
        assert_eq!(s.counts().outputs, 1);
        assert!(
            s.events[0]
                .text
                .as_deref()
                .unwrap()
                .contains("Permission denied")
        );
    }

    /// None of the three inner-content shapes may break the parse.
    ///
    /// Observed distribution: string 2200, `[{text}]` 7, `[{image}]` 4. An image has no text to
    /// take, but it **still counts as tool output** — failing to get a body must not fall back to
    /// treating the record as an ordinary user message, which records tool output as a user
    /// prompt.
    #[test]
    fn every_tool_result_content_shape_is_handled() {
        let text = r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":[{"type":"text","text":"batch_size: 512"}]}]},"sessionId":"s"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"b","content":[{"type":"image","source":{"type":"base64","data":"/9j/4AAQ"}}]}]},"sessionId":"s"}"#;
        let s = ClaudeCode.parse(text).unwrap();
        assert_eq!(s.counts().outputs, 1, "only a text block yields a body");
        assert_eq!(s.counts().prompts, 0, "image output is not a prompt");
        assert!(s.events[0].text.as_deref().unwrap().contains("512"));
    }

    #[test]
    fn corrupt_lines_are_skipped_not_fatal() {
        // A transcript can be truncated (the process was killed). A partial session is still
        // worth resuming.
        let text = "{\"type\":\"user\",\"message\":{\"content\":\"hi\"},\"sessionId\":\"s\"}\nNOT JSON\n{trunca";
        let s = ClaudeCode.parse(text).unwrap();
        assert_eq!(s.counts().prompts, 1, "bad line skipped, good line kept");
    }

    #[test]
    fn thinking_blocks_counted_as_dropped() {
        // Vendor-specific encrypted reasoning. It must count as dropped so the loss is
        // reportable.
        let text = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"..."}]},"sessionId":"s"}"#;
        let s = ClaudeCode.parse(text).unwrap();
        assert_eq!(s.counts().dropped, 1);
    }

    /// Every event remembers which line it came from — the web transcript uses that to go back
    /// to the source for tool arguments and thinking bodies ([`Event::line`]).
    #[test]
    fn events_point_back_at_their_source_line() {
        let text = concat!(
            r#"{"type":"user","sessionId":"s","message":{"role":"user","content":"q"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"thought"},{"type":"text","text":"a"},{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
            "\n",
        );
        let s = ClaudeCode.parse(text).unwrap();
        let lines: Vec<Option<usize>> = s.events.iter().map(|e| e.line).collect();
        assert_eq!(
            lines,
            vec![Some(0), Some(1), Some(1), Some(1)],
            "one line can produce several events, so line numbers are not unique: {lines:?}"
        );
        // Order within one line must be preserved — consumers pair on it against the blocks in
        // the source.
        assert_eq!(s.events[1].kind, EventKind::Other, "thinking is first");
        assert_eq!(s.events[2].kind, EventKind::AssistantReply);
        assert_eq!(s.events[3].kind, EventKind::ToolUse);
    }

    /// A line number is a position, not content, and must never enter a turn hash.
    ///
    /// Once it does, "the same conversation computes the same turn hash for two people" is gone —
    /// and that is the only reason [`crate::domain::turn`] exists.
    #[test]
    fn line_numbers_do_not_affect_turn_hashes() {
        let body = r#"{"type":"user","sessionId":"s","message":{"role":"user","content":"q"}}"#;
        let tight = ClaudeCode.parse(body).unwrap();
        // The same content with a few blank or corrupt lines in front — every line number
        // changes.
        let shifted = ClaudeCode.parse(&format!("\n\nNOT JSON\n{body}")).unwrap();
        assert_ne!(
            tight.events[0].line, shifted.events[0].line,
            "precondition: the line numbers differ"
        );
        assert_eq!(
            crate::domain::turn::chain_of(&tight).tip(),
            crate::domain::turn::chain_of(&shifted).tip(),
            "line numbers must not take part in normalization"
        );
    }

    #[test]
    fn render_chains_parent_uuids() {
        let s = Session {
            id: "src".into(),
            runtime: "codex".into(),
            cwd: None,
            events: vec![
                Event::text(EventKind::UserPrompt, "q", None),
                Event::text(EventKind::AssistantReply, "a", None),
            ],
        };
        let out = ClaudeCode.render(&s, "new-id", Path::new("/repo")).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2);
        let l0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let l1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert!(l0["parentUuid"].is_null(), "first record has no parent");
        assert_eq!(
            l1["parentUuid"].as_str().unwrap(),
            l0["uuid"].as_str().unwrap(),
            "the second record must hang off the first, or the conversation tree cannot be rebuilt"
        );
        assert_eq!(l0["sessionId"], "new-id");
    }

    #[test]
    fn render_timestamp_is_deterministic() {
        // Render must be a pure function, or repeated conversion shows git a file that changes
        // forever and delta compression stops working.
        let s = Session {
            id: "x".into(),
            runtime: "codex".into(),
            cwd: None,
            events: vec![Event::text(EventKind::UserPrompt, "q", None)],
        };
        let a = ClaudeCode.render(&s, "id", Path::new("/r")).unwrap();
        let b = ClaudeCode.render(&s, "id", Path::new("/r")).unwrap();
        let ta: serde_json::Value = serde_json::from_str(a.lines().next().unwrap()).unwrap();
        let tb: serde_json::Value = serde_json::from_str(b.lines().next().unwrap()).unwrap();
        assert_eq!(ta["timestamp"], tb["timestamp"]);
    }
    /// A compact summary must not be taken for the opening prompt.
    ///
    /// This pins a real bug: the summary's `type` and `role` are both `"user"`, so taking the
    /// first user message makes a resumed session show "This session is being continued…" in
    /// `agit log` instead of the first sentence the user actually typed — which destroys exactly
    /// what `agit log` is for: recognizing which piece of work a session is by its opening
    /// prompt.
    #[test]
    fn compact_summary_is_not_the_gist() {
        // The real shape: the summary first (that is what a resumed session looks like), the
        // real prompt after.
        let text = r#"{"type":"user","message":{"role":"user","content":"This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion."},"isCompactSummary":true,"parentUuid":null,"sessionId":"s1"}
{"type":"user","message":{"role":"user","content":"refactor payment error handling"},"sessionId":"s1","uuid":"u2"}"#;
        let s = ClaudeCode.parse(text).unwrap();

        assert_eq!(
            s.gist(40).as_deref(),
            Some("refactor payment error handling"),
            "gist must skip the compact summary and take the real user prompt"
        );

        let c = s.counts();
        assert_eq!(c.prompts, 1, "a summary is not a user prompt");
        assert_eq!(c.compactions, 1, "but it is counted on its own");
    }

    /// The summary body's format is not stable, so only the field can decide.
    #[test]
    fn compact_detection_relies_on_the_field_not_the_text() {
        // Of two observed samples one uses `Summary:` and the other wraps in `<summary>`, so
        // matching on the body text is guaranteed to miss.
        let with_field = r#"{"type":"user","message":{"role":"user","content":"any body at all"},"isCompactSummary":true,"sessionId":"s"}"#;
        assert_eq!(
            ClaudeCode.parse(with_field).unwrap().counts().compactions,
            1,
            "the field alone makes it a summary; the body is not read"
        );

        // The other way round: the body happens to contain that sentence but the field is
        // missing → a real user prompt.
        let without = r#"{"type":"user","message":{"role":"user","content":"This session is being continued from a previous conversation"},"sessionId":"s"}"#;
        let s = ClaudeCode.parse(without).unwrap();
        assert_eq!(s.counts().compactions, 0, "no field, no summary");
        assert_eq!(s.counts().prompts, 1);
    }

    /// A runtime-injected user message must not be taken for human typing.
    ///
    /// Of 549 plain-text user messages observed locally only 241 are human (44%). Without the
    /// filter, turn splitting cuts a pile of fake turns holding one event each, and the hash
    /// chain is where version IDs come from.
    #[test]
    fn injected_user_messages_are_not_prompts() {
        for s in [
            "<task-notification> <task-id>bh0rq7v78</task-id>",
            "<command-name>/model</command-name>",
            "<local-command-stdout>Set model to Opus 5",
            "<local-command-caveat>Caveat: The messages below",
            "[Your previous response had no visible output. Please continue",
            "[Request interrupted by user]",
            "[Request interrupted by user for tool use]",
            "A session-scoped Stop hook is now active with condition: \"…\"",
            // ── The nine shapes the second census added, pinned one by one. Observed shapes
            //    are in `is_synthetic_user_text`'s header, with the count for each.
            //
            // This one matters: the table has held `<command-name>` all along, but it sits on
            // the **second line** while the test is `starts_with`. 48 records slip past.
            "<command-message>init is analyzing your codebase…</command-message>\n\
             <command-name>/init</command-name>",
            "<user-prompt-submit-hook>\n🎯 **Claude Tip**: Try `/gh:push`\n</user-prompt-submit-hook>",
            "<session-start-hook>🔧 **Available MCP Tools**:</session-start-hook>",
            "<bash-input>pwd</bash-input>",
            "<bash-stdout>/home/dev/code</bash-stdout><bash-stderr></bash-stderr>",
            "<bash-notification>\n<shell-id>b09efb5</shell-id>\n</bash-notification>",
            // Attributes follow, so the prefix has no closing `>`.
            "<teammate-message teammate_id=\"team-lead\" color=\"green\">\n{}\n</teammate-message>",
            "<teammate-message teammate_id=\"arch-writer\">\n{}\n</teammate-message>",
            // The bare form of `<local-command-caveat>`.
            "Caveat: The messages below were generated by the user while running \
             local commands. DO NOT respond to these messages",
            "Your tool call was malformed and could not be parsed. Please retry.",
            "Unknown slash command: alignment",
        ] {
            assert!(is_synthetic_user_text(s), "must be an injection: {s}");
        }

        // Real user input must never be misjudged — agit log would stop recognizing what the
        // work was.
        for s in [
            "hi",
            "fix the album thumbnail rotation bug",
            "<div>fix this HTML for me</div>",
            "<T> how do I write this generic parameter",
            // The pasted-image note is followed by real words, so it is not an injection.
            "[Image: original 2100x1308, displayed at 2000x1246.]",
            // A short, ordinary sentence a user could well type. Under the test "better to miss
            // an injection shape than to judge real input synthetic", it stays out of the list.
            "Continue from where you left off.",
            "",
            // ── The three below are shapes the second census **deliberately excludes**, each
            //    with one piece of observed evidence ──
            //
            // `<ide_opened_file>` carries human words on its tail (two observed: `\nNow I would
            // like to add …` and `\nclaude plugin install figma@…`). Adding it to the table
            // makes those words disappear. The Codex-side table excludes it for the same
            // reason; this corpus reproduces that independently.
            "<ide_opened_file>The user opened the file /home/dev/x.md in the IDE.\
             </ide_opened_file>\nNow I would like to add a section",
            // A skill body, but its **tail is `ARGUMENTS: <user input>`**. One observed record
            // ends `ARGUMENTS: xoa het` (Vietnamese for "delete everything") — a human-typed
            // argument.
            "Base directory for this skill: /home/dev/.claude/plugins/x\n\n\
             # Mermaid Diagram Expert\n\nARGUMENTS: xoa het",
            // The expansion of `/init`. A human could well paste this sentence in.
            "Please analyze this codebase and create a CLAUDE.md file",
        ] {
            assert!(!is_synthetic_user_text(s), "must not be an injection: {s}");
        }
    }

    /// End to end: an injected user record lands in the IR as Other, not UserPrompt.
    #[test]
    fn injected_records_land_in_other_not_user_prompt() {
        let text = concat!(
            r#"{"type":"user","sessionId":"s","cwd":"/w","message":{"role":"user","content":"a real question"}}"#,
            "\n",
            r#"{"type":"user","sessionId":"s","message":{"role":"user","content":"<task-notification> done"}}"#,
            "\n",
            r#"{"type":"user","sessionId":"s","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
            "\n",
        );
        let s = ClaudeCode.parse(text).unwrap();
        let c = s.counts();
        assert_eq!(c.prompts, 1, "only one record is human");
        assert_eq!(c.dropped, 2, "both injections are recorded as Other");
        assert_eq!(s.gist(20).as_deref(), Some("a real question"));
    }

    /// A Skill tool's body reads like a user message, but `isMeta` marks it as not human.
    #[test]
    fn a_skill_meta_message_does_not_start_a_turn() {
        let text = concat!(
            r#"{"type":"user","sessionId":"s","message":{"role":"user","content":"upload yourself to AgentGitHub"}}"#,
            "\n",
            r#"{"type":"user","sessionId":"s","isMeta":true,"sourceToolUseID":"tool-1","message":{"role":"user","content":[{"type":"text","text":"Base directory for this skill: /Users/april/.claude/skills/agit\n\n# agit"}]}}"#,
            "\n",
        );
        let s = ClaudeCode.parse(text).unwrap();
        assert_eq!(s.counts().prompts, 1);
        assert_eq!(s.counts().dropped, 1);
        assert_eq!(crate::domain::turn::chain_of(&s).len(), 1);

        let typed = r#"{"type":"user","sessionId":"s","message":{"role":"user","content":"Base directory for this skill: how do I set that up?"}}"#;
        assert_eq!(ClaudeCode.parse(typed).unwrap().counts().prompts, 1);
    }

    /// **The record opening with a pasted-image note also carries `isMeta: true`, yet human
    /// words follow it.**
    ///
    /// The 4 records in the census have this shape (see the "Why not `isMeta`" section of
    /// [`is_synthetic_user_text`]): `isMeta` says "this record contains words the runtime added",
    /// not "the whole record is runtime-generated". Cutting on it alone loses the sentence the
    /// user asked alongside the image in three places at once: `counts().prompts` is one lower
    /// and `counts().dropped` one higher; `turn::chain_of` loses one turn boundary (shifting the
    /// whole version chain); and since `render` skips `EventKind::Other`, the restored session
    /// **no longer holds the instruction at all**.
    #[test]
    fn a_pasted_image_note_does_not_swallow_the_human_words_after_it() {
        let text = concat!(
            r#"{"type":"user","sessionId":"s","cwd":"/w","isMeta":true,"message":{"role":"user","#,
            r#""content":"[Image: original 2100x1308, displayed at 2000x1246.]\nfix the album thumbnail rotation bug"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","message":{"role":"assistant","content":[{"type":"text","text":"sure"}]}}"#,
            "\n",
        );
        let s = ClaudeCode.parse(text).unwrap();
        assert_eq!(s.counts().prompts, 1, "the words after the note are human");
        assert_eq!(s.counts().dropped, 0, "it is not a dropped injection");
        assert_eq!(
            crate::domain::turn::chain_of(&s).len(),
            1,
            "one UserPrompt fewer is one turn boundary fewer; the version chain shifts with it"
        );
        // The resume artifact must still hold this instruction: `render` skips
        // `EventKind::Other`.
        let out = ClaudeCode.render(&s, "new", Path::new("/w")).unwrap();
        assert!(
            out.contains("fix the album thumbnail rotation bug"),
            "the restored session must hold this user instruction:\n{out}"
        );
    }

    /// A turn the runtime continues on its own reads exactly like a human, so only
    /// `promptSource` recognizes it.
    ///
    /// The observed record is `Goal set: …`, whose body is the goal text the user set earlier.
    /// Any prefix rule that recognizes it also swallows the same sentence typed by a human — so
    /// the test has to be a structured field.
    #[test]
    fn a_system_generated_prompt_is_not_a_human_prompt() {
        let text = concat!(
            r#"{"type":"user","sessionId":"s","cwd":"/w","promptSource":"typed","message":{"role":"user","content":"fix CI"}}"#,
            "\n",
            r#"{"type":"user","sessionId":"s","promptSource":"system","origin":{"kind":"auto-continuation"},"message":{"role":"user","content":"Goal set: fix CI. Update the repo and the homepage."}}"#,
            "\n",
        );
        let s = ClaudeCode.parse(text).unwrap();
        let c = s.counts();
        assert_eq!(c.prompts, 1, "a runtime continuation is not human speech");
        assert_eq!(c.dropped, 1);
        assert_eq!(
            crate::domain::turn::chain_of(&s).len(),
            1,
            "and no second turn is cut"
        );

        // The same sentence typed by a human must still be a prompt — a distinction a prefix
        // rule cannot make.
        let typed = r#"{"type":"user","sessionId":"s","promptSource":"typed","message":{"role":"user","content":"Goal set: fix CI. Update the repo and the homepage."}}"#;
        assert_eq!(ClaudeCode.parse(typed).unwrap().counts().prompts, 1);
    }

    /// A missing `promptSource` is **not** an injection.
    ///
    /// Records from older builds carry no such field, and of 64 observed the overwhelming
    /// majority are real human input. Treating missing as injection erases all of them.
    #[test]
    fn a_missing_prompt_source_is_not_evidence_of_injection() {
        let old = r#"{"type":"user","sessionId":"s","message":{"role":"user","content":"human input with no promptSource"}}"#;
        assert_eq!(ClaudeCode.parse(old).unwrap().counts().prompts, 1);
        for src in ["typed", "sdk", "queued"] {
            let v: serde_json::Value = serde_json::json!({ "promptSource": src });
            assert!(
                !is_system_generated(&v, "human input with no promptSource"),
                "{src} is a human's intent"
            );
        }
    }

    /// An unknown record type must count as dropped and must not vanish silently.
    ///
    /// "How much the conversion loses" has to be reportable — that is the rule set at the top of
    /// adapter/mod.rs, and a bare `_ => {}` drops whole classes of record without a sound.
    #[test]
    fn unknown_record_types_are_counted_not_silently_dropped() {
        let text = concat!(
            r#"{"type":"user","sessionId":"s","message":{"role":"user","content":"a real prompt"}}"#,
            "\n",
            r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"server_tool_use","name":"web_search"}]}}"#,
            "\n",
        );
        let s = ClaudeCode.parse(text).unwrap();
        let c = s.counts();
        assert_eq!(c.prompts, 1);
        assert_eq!(
            c.dropped, 2,
            "an unknown top-level type and an unknown assistant block both count: {:?}",
            s.events
        );
        // The line number must come along so these records can be located in the source.
        let others: Vec<usize> = s
            .events
            .iter()
            .filter(|e| e.kind == EventKind::Other)
            .filter_map(|e| e.line)
            .collect();
        assert_eq!(others, vec![1, 2]);
    }

    /// The three extra record types in a Cowork (Claude Desktop) transcript must map to Other.
    ///
    /// Observed (docs/mechanism-probing/desktop-apps.md §3.2): `last-prompt` is a copy of the
    /// user prompt (one sentence appears 4 times), and a `queue-operation` enqueue's content is
    /// the prompt text as well. Taken as UserPrompt, one sentence is counted 6 times, and gist
    /// and turn splitting break together.
    #[test]
    fn cowork_record_types_map_to_other() {
        let prompt = "take a look at this diff";
        let mut lines = vec![format!(
            r#"{{"type":"user","sessionId":"s","message":{{"role":"user","content":"{prompt}"}}}}"#
        )];
        // The same prompt appears 4 times in last-prompt.
        for _ in 0..4 {
            lines.push(format!(
                r#"{{"type":"last-prompt","sessionId":"s","lastPrompt":"{prompt}","leafUuid":"u1"}}"#
            ));
        }
        lines.push(format!(
            r#"{{"type":"queue-operation","sessionId":"s","operation":"enqueue","content":"{prompt}"}}"#
        ));
        lines.push(
            r#"{"type":"attachment","sessionId":"s","attachment":{"type":"skill_listing"}}"#.into(),
        );
        let s = ClaudeCode.parse(&lines.join("\n")).unwrap();
        let c = s.counts();
        assert_eq!(c.prompts, 1, "a prompt is counted once: {}", c.prompts);
        assert_eq!(c.dropped, 6, "4 last-prompt + 1 enqueue + 1 attachment");
    }

    /// A compact summary is emitted: it is the legitimate opening context of a compacted session
    /// once it resumes.
    ///
    /// The render policy flipped (2026-08): the filtering decision sits with the per-branch
    /// resume VIEW built at commit (session/VIEW), and render's job is faithful reconstruction.
    /// The summary goes out as an ordinary user message (without isCompactSummary), and the
    /// parentUuid chain must stay unbroken across it.
    #[test]
    fn compact_summary_is_rendered_as_plain_resume_context() {
        let s = Session {
            id: "x".into(),
            runtime: "claude-code".into(),
            cwd: None,
            events: vec![
                Event::text(
                    EventKind::CompactSummary,
                    "This session is being continued…",
                    None,
                ),
                Event::text(EventKind::UserPrompt, "the real prompt", None),
            ],
        };
        let out = ClaudeCode.render(&s, "nid", Path::new("/r")).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "summary and real prompt both go out");
        assert!(out.contains("being continued"), "summary body is there");
        let l0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let l1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(l0["type"], "user");
        assert_eq!(l0["message"]["role"], "user");
        assert!(
            l0.get("isCompactSummary").is_none(),
            "no source-runtime boundary marker — to the target runtime this is ordinary context"
        );
        assert_eq!(
            l1["parentUuid"].as_str(),
            l0["uuid"].as_str(),
            "the parentUuid chain must span the summary, or the tree cannot be rebuilt"
        );
    }

    /// A filter boundary carries a label when emitted — its body is not conversation content.
    #[test]
    fn compact_filtered_is_rendered_as_a_labeled_note() {
        let s = Session {
            id: "x".into(),
            runtime: "claude-code".into(),
            cwd: None,
            events: vec![Event::text(
                EventKind::CompactFiltered,
                "context window #3, 17 messages kept",
                None,
            )],
        };
        let out = ClaudeCode.render(&s, "nid", Path::new("/r")).unwrap();
        assert!(
            out.contains("(this context was compact-filtered by the source runtime: context window #3, 17 messages kept)"),
            "the boundary note must be labeled, not emitted as bare text: {out}"
        );
    }

    /// A rendered compact summary must read back into the target runtime.
    ///
    /// The summary goes out as an ordinary user message (without isCompactSummary), so after a
    /// re-parse it survives as a UserPrompt — which is what "the legitimate opening context of a
    /// resumed session" means.
    #[test]
    fn rendered_compact_summary_is_valid_resume_context() {
        let s = Session {
            id: "x".into(),
            runtime: "claude-code".into(),
            cwd: None,
            events: vec![
                Event::text(
                    EventKind::CompactSummary,
                    "this is the compacted context summary",
                    None,
                ),
                Event::text(
                    EventKind::CompactFiltered,
                    "context window #2, 5 messages kept",
                    None,
                ),
                Event::text(EventKind::UserPrompt, "keep going", None),
            ],
        };
        let out = ClaudeCode.render(&s, "nid", Path::new("/r")).unwrap();
        let back = ClaudeCode.parse(&out).unwrap();
        assert_eq!(back.id, "nid");
        let texts: Vec<&str> = back
            .events
            .iter()
            .filter_map(|e| e.text.as_deref())
            .collect();
        assert!(
            texts
                .iter()
                .any(|t| t.contains("compacted context summary")),
            "the summary must survive the round trip: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains("compact-filtered by the source runtime")),
            "the filter boundary must survive the round trip with its label: {texts:?}"
        );
        assert!(texts.iter().any(|t| t.contains("keep going")));
    }

    /// Words the user throws in mid-turn: only the `queue-operation` `remove`
    /// (`reason: absorbed_mid_turn`) keeps them verbatim in the transcript, with no `user`
    /// record. They must enter the IR (or the sentence does not exist in the VIEW), but must not
    /// open a new turn and must not count toward prompts.
    #[test]
    fn a_message_absorbed_mid_turn_is_an_interjection_not_a_prompt() {
        let lines = [
            r#"{"type":"user","sessionId":"s","timestamp":"2026-08-29T11:00:00.000Z","message":{"role":"user","content":"merge this branch for me"}}"#.to_string(),
            r#"{"type":"assistant","sessionId":"s","timestamp":"2026-08-29T11:00:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"check the status first"},{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"git status"}}]}}"#.to_string(),
            r#"{"type":"queue-operation","operation":"enqueue","timestamp":"2026-08-29T11:00:02.000Z","sessionId":"s","content":"also update the README"}"#.to_string(),
            r#"{"type":"user","sessionId":"s","timestamp":"2026-08-29T11:00:03.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"clean\n\nThe user sent a new message while you were working:\nalso update the README\n\nThis is how Claude Code surfaces messages the user sends mid-turn"}]}}"#.to_string(),
            r#"{"type":"queue-operation","operation":"remove","timestamp":"2026-08-29T11:00:03.100Z","sessionId":"s","reason":"absorbed_mid_turn","content":"also update the README"}"#.to_string(),
            r#"{"type":"assistant","sessionId":"s","timestamp":"2026-08-29T11:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"ok, both at once"}]}}"#.to_string(),
        ];
        let s = ClaudeCode.parse(&lines.join("\n")).unwrap();
        let c = s.counts();
        assert_eq!(c.prompts, 1, "an interjection is not a prompt: {c:?}");
        assert_eq!(c.interjections, 1, "{c:?}");
        // The enqueue copy is still a copy and still counts as dropped.
        assert_eq!(c.dropped, 1, "{c:?}");
        let interjection = s
            .events
            .iter()
            .find(|e| e.kind == EventKind::UserInterjection)
            .expect("an interjection enters the IR");
        assert_eq!(interjection.text.as_deref(), Some("also update the README"));
        assert_eq!(interjection.line, Some(4), "it points at the remove line");
        // It lands after the tool output and before the reply — that is where the model read
        // it.
        let kinds: Vec<EventKind> = s.events.iter().map(|e| e.kind).collect();
        let pos = |k: EventKind| kinds.iter().position(|x| *x == k).unwrap();
        assert!(pos(EventKind::ToolResult) < pos(EventKind::UserInterjection));
        assert!(pos(EventKind::UserInterjection) < kinds.len() - 1);
        // One turn only.
        assert_eq!(crate::domain::turn::groups_of(&s).len(), 1);
    }

    /// A background-task notice goes through the same queue and is absorbed into the current
    /// turn too, but it is not something a human said. A message the user retracts (any other
    /// `reason`) never entered the conversation and does not count either.
    #[test]
    fn absorbed_notifications_and_other_removals_stay_out() {
        let lines = [
            r#"{"type":"user","sessionId":"s","message":{"role":"user","content":"start"}}"#,
            r#"{"type":"queue-operation","operation":"remove","sessionId":"s","reason":"absorbed_mid_turn","content":"<task-notification>\n<task-id>x</task-id>\n</task-notification>"}"#,
            r#"{"type":"queue-operation","operation":"remove","sessionId":"s","reason":"absorbed_mid_turn","content":"<system-reminder>\nreminder\n</system-reminder>"}"#,
            r#"{"type":"queue-operation","operation":"remove","sessionId":"s","reason":"cleared","content":"never mind"}"#,
            r#"{"type":"queue-operation","operation":"dequeue","sessionId":"s","content":null}"#,
        ];
        let s = ClaudeCode.parse(&lines.join("\n")).unwrap();
        let c = s.counts();
        assert_eq!(c.prompts, 1, "{c:?}");
        assert_eq!(c.interjections, 0, "{c:?}");
        assert_eq!(c.dropped, 4, "{c:?}");
    }

    /// Installing back into Claude Code emits an interjection as an ordinary user message: the
    /// target runtime has no "absorbed into the current turn" form, so all that restores is "the
    /// user said this at this position".
    #[test]
    fn interjections_render_as_plain_user_messages() {
        let s = Session {
            id: "s".into(),
            runtime: "claude-code".into(),
            cwd: None,
            events: vec![
                Event::text(EventKind::UserPrompt, "start", None),
                Event::text(EventKind::AssistantReply, "on it", None),
                Event::text(EventKind::UserInterjection, "also update README", None),
                Event::text(EventKind::AssistantReply, "doing both", None),
            ],
        };
        let out = ClaudeCode.render(&s, "n", Path::new("/w")).unwrap();
        let back = ClaudeCode.parse(&out).unwrap();
        let texts: Vec<(EventKind, String)> = back
            .events
            .iter()
            .map(|e| (e.kind, e.text.clone().unwrap_or_default()))
            .collect();
        assert!(
            texts.contains(&(EventKind::UserPrompt, "also update README".into())),
            "{texts:?}"
        );
        // The chain must not break: every record follows the one before it.
        let recs: Vec<serde_json::Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        for w in recs.windows(2) {
            assert_eq!(w[1]["parentUuid"], w[0]["uuid"]);
        }
    }
}
