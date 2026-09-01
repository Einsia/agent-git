//! Codex adapter.
//!
//! # On-disk format
//!
//! ```text
//! ~/.codex/sessions/YYYY/MM/DD/rollout-<ISO>-<uuid>.jsonl
//! ```
//!
//! One directory per date, **with no project information in the path**. Direct consequence:
//! finding "the sessions that belong to a repo" means opening each file and reading
//! `session_meta.cwd`. This is the most important structural difference from Claude Code, and it
//! is why `read_head` exists.
//!
//! Every line is `{type, payload}`. The types that matter: `session_meta` (cwd/id),
//! `response_item` (message / function_call / custom_tool_call / reasoning), `event_msg` +
//! `patch_apply_end` (file edits).
//!
//! A tool call and its output are two records paired by `call_id`: `function_call` ↔
//! `function_call_output`, `custom_tool_call` ↔ `custom_tool_call_output`, `local_shell_call` ↔
//! `local_shell_call_output`. A call that never gets its output means the turn is still running
//! ([`Adapter::open_tool_calls`]).
//!
//! # Resume
//!
//! ```bash
//! codex resume <session-id>
//! ```
//!
//! Scans `sessions/` recursively and matches by id, so a rollout is found whichever date directory
//! it was written to. `codex exec resume <id>` requires a prompt and fails without one, so use the
//! interactive form.

use super::{
    Adapter, Capability, Event, EventKind, Installed, Next, OpenCall, Session, SessionRef,
};
use crate::Result;
use anyhow::Context;
use std::path::{Path, PathBuf};

pub struct Codex;

/// The output body paired to a tool call replayed from another runtime.
///
/// The real output on the source side cannot be attributed to this call (the IR does not model
/// the pairing), but Codex refuses a `function_call` with no output; this sentence tells the
/// resumed agent what is missing here instead of passing an empty string off as "the command
/// produced no output".
pub const CROSS_RUNTIME_OUTPUT_PLACEHOLDER: &str =
    "[agit] this tool call was replayed from another runtime; its output was not carried over.";

fn sessions_root() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("$HOME is not set")?;
    Ok(PathBuf::from(home).join(".codex").join("sessions"))
}

/// Read only the leading bytes of a file.
///
/// **This function is performance-critical.** `session_meta` is always on the first line, while a
/// transcript can run to tens of MB. Reading the whole file degrades "list the sessions that
/// belong to this repo" to minutes on a machine with many sessions (especially with `~/.codex` on
/// a network filesystem).
fn read_head(path: &Path, bytes: usize) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; bytes];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    // The cut can land inside a multi-byte character, so decode lossily (`from_utf8` would fail
    // on the whole buffer).
    Some(String::from_utf8_lossy(&buf).to_string())
}

/// Parse (id, cwd) out of the file header.
fn primary_meta(path: &Path) -> Option<(String, String)> {
    let head = read_head(path, 8192)?;
    for line in head.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            // The last line of the header can be cut off by `read_head`; a parse failure here
            // is expected.
            continue;
        };
        if v.get("type").and_then(|x| x.as_str()) != Some("session_meta") {
            continue;
        }
        let p = v.get("payload")?;
        let cwd = p.get("cwd").and_then(|x| x.as_str()).unwrap_or("");
        if cwd.is_empty() {
            return None;
        }
        let id = p
            .get("id")
            .and_then(|x| x.as_str())
            .map(String::from)
            .or_else(|| id_from_filename(path))?;
        return Some((id, cwd.to_string()));
    }
    None
}

/// Take the uuid out of `rollout-2026-07-25T18-20-01-<uuid>.jsonl`.
///
/// The uuid is the last five `-`-separated segments (8-4-4-4-12); the timestamp is in front of it.
fn id_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 5 {
        return None;
    }
    Some(parts[parts.len() - 5..].join("-"))
}

fn all_rollouts(root: &Path) -> Vec<PathBuf> {
    if !root.exists() {
        return vec![];
    }
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
        .collect()
}

/// One row of the index database → `SessionRef`.
///
/// mtime comes from the database's `updated_at_ms` rather than a stat of the file — sorting must
/// not stat tens of thousands of files (that is exactly the cost this path removes).
fn thread_to_ref(t: super::codex_index::Thread) -> SessionRef {
    let mtime = t
        .updated_at_ms
        .and_then(|ms| u64::try_from(ms.max(0)).ok())
        .map(|ms| std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms))
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    SessionRef {
        id: t.id,
        path: t.rollout_path,
        runtime: "codex",
        cwd: t.cwd,
        mtime,
        // Already in the index, so it is free — without it the caller reparses the whole
        // transcript.
        gist: t.first_user_message,
    }
}

/// List every session by scanning files — the fallback when the index database is unusable.
///
/// **Cost warning**: linear in the total number of sessions, each costing an 8 KB header read. On
/// this machine, 18745 files on NFS takes minutes. It stays because the index database may not
/// exist (Codex not installed, database not built yet) or its schema may have changed.
fn scan_all_sessions() -> Result<Vec<SessionRef>> {
    let root = sessions_root()?;
    let mut out = vec![];
    for p in all_rollouts(&root) {
        let Some((id, cwd)) = primary_meta(&p) else {
            continue;
        };
        let mtime = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        out.push(SessionRef {
            id,
            path: p,
            runtime: "codex",
            cwd: Some(cwd),
            mtime,
            gist: None,
        });
    }
    Ok(out)
}

impl Adapter for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn cli(&self) -> &'static str {
        "codex"
    }

    fn capability(&self) -> Capability {
        Capability::Resumable
    }

    fn format(&self) -> &'static str {
        // ChatGPT Desktop (com.openai.codex) reads and writes the same CODEX_HOME — one format
        // family, not two (desktop-apps.md §3.1).
        "codex"
    }

    /// List the sessions whose cwd equals `repo`.
    ///
    /// The cost is linear in the **total** number of sessions (every header is read to decide its
    /// cwd). `read_head` holds the per-file cost down to 8KB, but the file count is unavoidable.
    /// So callers stay restrained: use it only in commands the user triggers, such as `doctor` and
    /// `push`.
    fn sessions_for(&self, repo: &Path) -> Result<Vec<SessionRef>> {
        let want = repo.to_string_lossy().to_string();

        // Ask the index database first (0.8 ms). A missing database, a changed schema, or Codex
        // not being installed all return None — then it falls back to scanning files, which
        // changes nothing but the speed.
        if let Some(threads) = super::codex_index::threads_for_cwd(&want) {
            return Ok(threads.into_iter().map(thread_to_ref).collect());
        }

        Ok(self
            .all_sessions()?
            .into_iter()
            .filter(|s| s.cwd.as_deref() == Some(want.as_str()))
            .collect())
    }

    fn all_sessions(&self) -> Result<Vec<SessionRef>> {
        if let Some(threads) = super::codex_index::all_threads() {
            return Ok(threads.into_iter().map(thread_to_ref).collect());
        }
        scan_all_sessions()
    }

    /// Reverse lookup: the index database first (0.40 ms), then a glob by id (39 ms).
    ///
    /// `cwd` is useless here — Codex splits directories by date and the path carries no project
    /// information.
    fn resolve(&self, session_id: &str, _cwd: Option<&Path>) -> Option<PathBuf> {
        if let Some(t) = super::codex_index::thread_by_id(session_id)
            && t.rollout_path.is_file()
        {
            return Some(t.rollout_path);
        }
        // Last resort: the filename has the form `rollout-<ISO>-<uuid>.jsonl`, with the id
        // embedded in it.
        let root = sessions_root().ok()?;
        all_rollouts(&root)
            .into_iter()
            .find(|p| id_from_filename(p).as_deref() == Some(session_id))
    }

    fn parse(&self, text: &str) -> Result<Session> {
        let mut id = String::new();
        let mut cwd = None;
        let mut events = vec![];
        // Whether any message has been seen before this point: it decides whether a compacted
        // record's replacement_history is "a duplicate" or "the only carrier" (see the
        // compacted arm).
        let mut saw_message = false;

        // The line number goes into every event ([`Event::line`]); the web transcript uses it to
        // go back to the source for tool arguments and reasoning. Blank and malformed lines take
        // a number too, so the coordinate addresses that exact line in the file.
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue; // As in Claude Code: skip a malformed line
            };
            let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
            let payload = v.get("payload");
            let ts = v
                .get("timestamp")
                .and_then(|x| x.as_str())
                .map(String::from);

            match ty {
                // ── compact: a **top-level** record type, not an event_msg subtype ──
                //
                // A probe that filters on `payload.type` misses the whole mechanism: a
                // `compacted` record carries its type at the top level and its payload has no
                // type field, so the only thing that shows up is the `event_msg/context_compacted`
                // notice for the UI that follows it (payload carries only a type, no body) — read
                // as "there is no summary".
                //
                // One compact appends five records:
                //   compacted                    ← the real payload, it is here
                //   world_state                  ← environment snapshot
                //   turn_context                 ← new turn
                //   event_msg/token_count
                //   event_msg/context_compacted  ← only a UI notice
                //
                // `replacement_history` is a **structured message array** and user input is kept
                // verbatim (every sampled message matches exactly), so this is CompactFiltered
                // and not Summary.
                "compacted" => {
                    let p = payload;
                    let rh = p
                        .and_then(|p| p.get("replacement_history"))
                        .and_then(|x| x.as_array());
                    let kept = rh.map(|a| a.len()).unwrap_or(0);
                    let window = p
                        .and_then(|p| p.get("window_number"))
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0);
                    events.push(
                        Event::text(
                            EventKind::CompactFiltered,
                            format!("context window #{window}, {kept} messages kept"),
                            ts.clone(),
                        )
                        .at_line(lineno),
                    );
                    // In a complete transcript the body of replacement_history already appeared
                    // verbatim before this record, so only the boundary description is kept and
                    // nothing is duplicated into the IR. But in a text that opens at a boundary
                    // (a branch VIEW's resume view) it is the only carrier of the retained
                    // context — without expanding it, the resumed session has not even an
                    // opening prompt.
                    if !saw_message && let Some(rh) = rh {
                        for m in rh {
                            if m.get("type").and_then(|x| x.as_str()) != Some("message") {
                                continue;
                            }
                            let role = m.get("role").and_then(|x| x.as_str()).unwrap_or("");
                            if let Some(t) = extract_content_text(m.get("content")) {
                                let (kind, text) = classify_message(role, t);
                                events.push(Event::text(kind, text, ts.clone()).at_line(lineno));
                            }
                        }
                        saw_message = kept > 0;
                    }
                }
                "session_meta" => {
                    if let Some(p) = payload {
                        if id.is_empty()
                            && let Some(s) = p.get("id").and_then(|x| x.as_str())
                        {
                            id = s.to_string();
                        }
                        if cwd.is_none()
                            && let Some(c) = p
                                .get("cwd")
                                .and_then(|x| x.as_str())
                                .filter(|c| !c.is_empty())
                        {
                            cwd = Some(c.to_string());
                        }
                    }
                }
                "response_item" => {
                    let Some(p) = payload else { continue };
                    match p.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                        "message" => {
                            saw_message = true;
                            let role = p.get("role").and_then(|x| x.as_str()).unwrap_or("");
                            if let Some(t) = extract_content_text(p.get("content")) {
                                let (kind, text) = classify_message(role, t);
                                events.push(Event::text(kind, text, ts).at_line(lineno));
                            }
                        }
                        "function_call" | "local_shell_call" | "custom_tool_call" => {
                            let name = p
                                .get("name")
                                .and_then(|x| x.as_str())
                                .unwrap_or("shell")
                                .to_string();
                            events.push(Event {
                                kind: EventKind::ToolUse,
                                text: Some(name.clone()),
                                timestamp: ts,
                                paths: vec![],
                                tool: Some(name),
                                line: Some(lineno),
                            });
                        }
                        "function_call_output"
                        | "local_shell_call_output"
                        | "custom_tool_call_output" => {
                            // Codex writes tool results as a separate response item.  They
                            // are evidence, not user prompts; dropping them makes
                            // `in:output` disagree with the Claude adapter.
                            if let Some(text) = extract_output_text(p.get("output")) {
                                events.push(
                                    Event::text(EventKind::ToolResult, text, ts).at_line(lineno),
                                );
                            } else {
                                // Keep an untextual/unknown output visible in loss accounting
                                // instead of silently treating it as if it never existed.
                                events.push(other_event(ts).at_line(lineno));
                            }
                        }
                        // reasoning is encrypted and vendor-proprietary; the IR cannot express
                        // it. Recorded as Other so the loss can be reported.
                        "reasoning" => events.push(Event {
                            kind: EventKind::Other,
                            text: None,
                            timestamp: ts,
                            paths: vec![],
                            tool: None,
                            line: Some(lineno),
                        }),
                        // An unknown payload type (a custom tool call, or any form added
                        // later) counts toward the dropped total too, never silently.
                        _ => events.push(other_event(ts).at_line(lineno)),
                    }
                }
                "event_msg" => {
                    let Some(p) = payload else { continue };
                    // This turn is done. There is one per human turn (a turn still running has
                    // none), so it is a reliable close signal.
                    if p.get("type").and_then(|x| x.as_str()) == Some("task_complete") {
                        events.push(Event {
                            kind: EventKind::TurnEnd,
                            text: None,
                            timestamp: ts.clone(),
                            paths: vec![],
                            tool: None,
                            line: Some(lineno),
                        });
                        continue;
                    }
                    if p.get("type").and_then(|x| x.as_str()) == Some("patch_apply_end") {
                        let paths: Vec<String> = p
                            .get("changes")
                            .and_then(|c| c.as_object())
                            .map(|o| o.keys().cloned().collect())
                            .unwrap_or_default();
                        if !paths.is_empty() {
                            events.push(Event {
                                kind: EventKind::FileEdit,
                                text: Some(format!("edited {} files", paths.len())),
                                timestamp: ts,
                                paths,
                                tool: Some("apply_patch".into()),
                                line: Some(lineno),
                            });
                        }
                    }
                }
                // An unknown top-level record type (world_state, turn_context, anything added
                // later) counts toward the dropped total too, never silently.
                _ => events.push(other_event(ts).at_line(lineno)),
            }
        }

        Ok(Session {
            id,
            runtime: "codex".into(),
            cwd,
            events,
        })
    }

    fn open_tool_calls(&self, text: &str) -> Vec<OpenCall> {
        let mut open: Vec<OpenCall> = Vec::new();
        for (lineno, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if v.get("type").and_then(|x| x.as_str()) != Some("response_item") {
                continue;
            }
            let Some(p) = v.get("payload") else { continue };
            let record = p.get("type").and_then(|x| x.as_str()).unwrap_or("");
            let call_id = p.get("call_id").and_then(|x| x.as_str());
            match (record, call_id) {
                // Nothing can pair a call with no call_id; treat it as closed, not as open.
                ("function_call" | "local_shell_call" | "custom_tool_call", Some(id)) => {
                    open.push(OpenCall {
                        line: lineno,
                        call_id: id.to_string(),
                        record: record.to_string(),
                        name: p
                            .get("name")
                            .and_then(|x| x.as_str())
                            .unwrap_or("shell")
                            .to_string(),
                    });
                }
                (
                    "function_call_output" | "local_shell_call_output" | "custom_tool_call_output",
                    Some(id),
                ) => open.retain(|c| c.call_id != id),
                _ => {}
            }
        }
        open
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
        // A fixed timestamp: rendering must be a pure function (see the comment of the same
        // name in claude_code).
        let now = "2026-01-01T00:00:00.000Z";
        let mut minted = 0usize;

        // The first line must be session_meta; Codex bootstraps from it.
        //
        // `payload.timestamp` is a required field for Codex to parse session_meta: without it the
        // line is not a session_meta at all, `codex resume` refuses to open the file with
        // "rollout does not start with session metadata", and it backfills a thread with an empty
        // cwd / cli_version into its index. The top-level `timestamp` is the envelope field every
        // record carries; write both.
        //
        // `source` decides which entry point this session belongs to in Codex's index; leaving it
        // out makes it vscode.
        //
        // `model_provider` is hard-coded to "openai" (the Codex default). A user on a non-openai
        // backend overrides it at launch: `codex resume <id> -c model_provider=<x>`.
        out.push_str(&serde_json::to_string(&serde_json::json!({
            "type": "session_meta",
            "timestamp": now,
            "payload": {
                "id": new_id,
                "timestamp": now,
                "cwd": cwd_s,
                "originator": "agit",
                "cli_version": env!("CARGO_PKG_VERSION"),
                "source": "cli",
                "model_provider": "openai",
            }
        }))?);
        out.push('\n');

        for (ei, e) in session.events.iter().enumerate() {
            let ts = e.timestamp.clone().unwrap_or_else(|| now.to_string());
            let rec = match e.kind {
                EventKind::UserPrompt
                | EventKind::UserInterjection
                | EventKind::AssistantReply
                | EventKind::CompactSummary
                | EventKind::CompactFiltered => {
                    // compact boundaries **are emitted**.
                    //
                    // Why (same as the arm of the same name in claude_code.rs): the filtering
                    // decision sits in each branch's resume view (session/VIEW, built at commit
                    // time), so render's only job is faithful reconstruction — and a compact
                    // summary is exactly the legitimate starting context of a compacted session
                    // once resumed; withholding it fakes a session with no beginning.
                    //
                    // Both a summary and a filtered boundary go out as user/input_text: the
                    // resumed session needs them as context. A CompactFiltered body is not
                    // conversation content (it marks what the source runtime filtered), so it
                    // must carry an explicit label, letting the agent see it as a boundary note
                    // rather than something the user said.
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
                    let is_user = e.kind != EventKind::AssistantReply;
                    out.push_str(&serde_json::to_string(&serde_json::json!({
                        "type": "response_item",
                        "timestamp": ts,
                        "payload": {
                            "type": "message",
                            "role": if is_user { "user" } else { "assistant" },
                            "content": [{
                                // input_text/output_text carry the direction, a Codex convention.
                                "type": if is_user { "input_text" } else { "output_text" },
                                "text": text
                            }]
                        }
                    }))?);
                    out.push('\n');
                    // The same sentence goes out again as an `event_msg`: when Codex resumes a
                    // session, the history visible in the interface is rebuilt from event_msg
                    // alone, while `response_item` only feeds the model. Write the latter only
                    // and the model remembers everything while the person opens a blank screen.
                    // Codex writes both every turn as well.
                    if is_user {
                        serde_json::json!({
                            "type": "event_msg",
                            "timestamp": ts,
                            "payload": {
                                "type": "user_message",
                                "message": text,
                                "images": [],
                                "local_images": [],
                                "audio": [],
                                "local_audio": [],
                                "text_elements": []
                            }
                        })
                    } else {
                        serde_json::json!({
                            "type": "event_msg",
                            "timestamp": ts,
                            "payload": {
                                "type": "agent_message",
                                "message": text,
                                "phase": "final_answer",
                                "memory_citation": null
                            }
                        })
                    }
                }
                EventKind::ToolUse | EventKind::FileEdit => {
                    let d = details.get(ei);
                    // A receipt-shaped edit (flagged by the source extractor, see
                    // ToolDetails::receipts): minting a call pair invents a second edit plus a
                    // placeholder output, so keep only the change signal.
                    if e.kind == EventKind::FileEdit && details.is_receipt(ei) {
                        let changes: serde_json::Map<String, serde_json::Value> = e
                            .paths
                            .iter()
                            .map(|p| (p.clone(), serde_json::json!({})))
                            .collect();
                        out.push_str(&serde_json::to_string(&serde_json::json!({
                            "type": "event_msg",
                            "timestamp": ts,
                            "payload": { "type": "patch_apply_end", "changes": changes, "success": true }
                        }))?);
                        out.push('\n');
                        continue;
                    }
                    // A call replayed across runtimes must bring its paired output: Codex
                    // refuses a `function_call` that never gets a `function_call_output`. The
                    // call_id is minted and only has to be unique within this artifact.
                    // Arguments and output come back from enrich, which reads the source
                    // transcript by `Event.line`; when they cannot be recovered the arguments are
                    // empty and the output is the placeholder — whose wording says what is
                    // missing, so it is not taken for a real output.
                    minted += 1;
                    let call_id = format!("agit-call-{minted}");
                    let arguments = d
                        .and_then(|d| d.input.as_ref())
                        .map(|v| match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .unwrap_or_else(|| match e.paths.first() {
                            Some(p) => serde_json::json!({ "file_path": p }).to_string(),
                            None => "{}".into(),
                        });
                    let output = d
                        .and_then(|d| d.output.clone())
                        .unwrap_or_else(|| CROSS_RUNTIME_OUTPUT_PLACEHOLDER.into());
                    out.push_str(&serde_json::to_string(&serde_json::json!({
                        "type": "response_item",
                        "timestamp": ts,
                        "payload": {
                            "type": "function_call",
                            "call_id": call_id,
                            "name": e.tool.clone().unwrap_or_else(|| "tool".into()),
                            "arguments": arguments
                        }
                    }))?);
                    out.push('\n');
                    out.push_str(&serde_json::to_string(&serde_json::json!({
                        "type": "response_item",
                        "timestamp": ts,
                        "payload": {
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": output
                        }
                    }))?);
                    out.push('\n');
                    if e.kind != EventKind::FileEdit {
                        continue;
                    }
                    // An edit event also gets a file-change signal (Codex's native patch flow
                    // carries both the call pair and patch_apply_end), with success reported as
                    // it was. The values are empty objects — the IR keeps no diff content, which
                    // is part of what is lossy across vendors.
                    let changes: serde_json::Map<String, serde_json::Value> = e
                        .paths
                        .iter()
                        .map(|p| (p.clone(), serde_json::json!({})))
                        .collect();
                    serde_json::json!({
                        "type": "event_msg",
                        "timestamp": ts,
                        "payload": {
                            "type": "patch_apply_end",
                            "changes": changes,
                            "success": !d.is_some_and(|d| d.error)
                        }
                    })
                }
                // The target runtime writes task_complete itself; it must not be replayed out of
                // the IR. A tool output is not replayed either: it has to pair with a
                // `function_call`, and the IR does not model that pairing — emitted alone it is an
                // ownerless output. See `EventKind::ToolResult`.
                EventKind::ToolResult | EventKind::TurnEnd | EventKind::Other => continue,
            };
            out.push_str(&serde_json::to_string(&rec)?);
            out.push('\n');
        }
        Ok(out)
    }

    fn mint_id(&self) -> String {
        uuid::Uuid::now_v7().to_string()
    }

    fn install(&self, content: &str, new_id: &str, _cwd: &Path) -> Result<Installed> {
        // Write into today's date directory. The date itself does not matter — Codex scans
        // recursively and matches by id.
        let now = chrono::Utc::now();
        let dir = sessions_root()?
            .join(now.format("%Y").to_string())
            .join(now.format("%m").to_string())
            .join(now.format("%d").to_string());
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))?;
        let path = dir.join(format!(
            "rollout-{}-{new_id}.jsonl",
            now.format("%Y-%m-%dT%H-%M-%S")
        ));
        std::fs::write(&path, content)
            .with_context(|| format!("cannot write {}", path.display()))?;
        Ok(Installed {
            next: Next::Resume(format!("codex resume {new_id}")),
            path,
        })
    }
}

/// What a `response_item/message` is recorded as, and which span of it is the body.
///
/// Returns `(kind, text)` rather than kind alone, because Codex's attachment wrapper (see
/// [`wrapped_human_request`]) needs the span the human actually typed peeled out of the wrapper.
///
/// # The role test: only `assistant` is the model speaking
///
/// Classifying by `if role != "user" { AssistantReply }` renders a `role=developer` record onto
/// the page as a model reply — that is the runtime instructing the model, not the model speaking,
/// and printing it as the agent's prose misreports what happened in this conversation.
///
/// Across 108 transcripts on this machine, 4855 messages: `assistant` 3654, `user` 977,
/// `developer` 224. **Not one** of the 224 `developer` messages is a human or the model speaking:
///
/// | count | content |
/// |---|---|
/// | 92 | `<permissions instructions>` sandbox description |
/// | 56 | `You are ..., the primary agent in a team of agents...` multi-agent formation orders |
/// | 48 | `<multi_agent_mode>` delegation switch |
/// | 11 | `<app-context>` desktop environment |
/// | 9 | `<skills_instructions>` skill description |
/// | 6 | `<turn_aborted>` interruption notice |
/// | 2 | `<collaboration_mode>` collaboration mode |
///
/// So the test is an allowlist and not a denylist: **an unknown role is always `Other`**. With a
/// denylist, the next role Codex introduces gets printed as the agent's words all over again;
/// with an allowlist, the worst outcome is that it lands in `dropped` (the count runs high, but
/// nobody is accused of saying something they never said).
pub(crate) fn classify_message(role: &str, text: String) -> (EventKind, String) {
    if role == "assistant" {
        return (EventKind::AssistantReply, text);
    }
    if role != "user" {
        // developer / system / any new role later: the runtime speaking, not conversation
        // content.
        return (EventKind::Other, text);
    }
    let inner = wrapped_human_request(&text).map(str::to_string);
    match inner {
        // The wrapper holds text a human typed → that text is the prompt; the list around it is
        // runtime-generated.
        Some(inner) if !is_only_attachments(&inner) => (EventKind::UserPrompt, inner),
        // The wrapper holds only attachments (the user dropped an image and typed nothing) →
        // there is no prompt.
        Some(_) => (EventKind::Other, text),
        None if is_synthetic_user_text(&text) => (EventKind::Other, text),
        None => (EventKind::UserPrompt, text),
    }
}

/// Codex wraps an attachment list / selected text / annotations **in front of** what the human
/// actually typed, separated by a fixed marker line. Returns the span after that marker line.
///
/// # Why peel and not classify the whole message as an injection
///
/// `# Files mentioned by the user:` is already in the injected-prefix table, so out of 76 such
/// messages on this machine **73 had the text a human typed erased outright** — not a prompt, not
/// a turn boundary, not usable as a gist. That is exactly the failure direction injection
/// detection must avoid ("better to miss a new form of injection than to classify real user input
/// as synthetic", as the comment there says), and this entry walked straight into it.
///
/// Peeling uses **the runtime's own separator**, not one more guessed heuristic: Codex says in so
/// many words "here is the user's request". And it peels only when the whole message starts with
/// a known wrapper, so a genuine human message that happens to contain that sentence is never
/// truncated.
///
/// With no marker line it returns `None` and the caller falls back to the prefix table —
/// `# Files mentioned by the user:` is still in that table, and then the message really is
/// nothing but the list.
fn wrapped_human_request(text: &str) -> Option<&str> {
    /// The wrappers. All appear in real transcripts; each opens with a list or a selection and
    /// ends with the human's request.
    const WRAPPERS: &[&str] = &[
        // Files and images the user dropped in (76 observed)
        "# Files mentioned by the user:",
        // Text the user selected in the IDE (1 observed)
        "# Selected text:",
        // Annotations the user made on the previous reply (1 observed)
        "# Response annotations:",
        // Editor state injected by the IDE plugin: current file, open tabs, selected lines (218
        // observed, spread across 31 sessions). **Not peeling this does not inflate a count, it
        // mislabels the outcome**: the header runs 291 to 2909 characters while the human's actual
        // question is a few dozen, so when one person asks two questions in a row about the same
        // file, the bigram Jaccard of the two messages lands between 0.90 and 1.00 — "the same
        // question was asked N times over" reads that number as `Failed`, while none of those 31
        // sessions was actually stuck. Auditing 20 `failed` labels, three trace back to this one
        // (oc-19 / oc-28 / oc-33 in `eval/outcomes_to_label.jsonl`).
        "# Context from my IDE setup:",
    ];
    /// The marker line Codex writes itself.
    const REQUEST_MARK: &str = "## My request for Codex:";

    let s = text.trim_start();
    if !WRAPPERS.iter().any(|w| s.starts_with(w)) {
        return None;
    }
    Some(s.split_once(REQUEST_MARK)?.1.trim())
}

/// Whether what is left after peeling the wrapper is nothing but attachment references.
///
/// When the user drops one image and types nothing, all that follows the marker line is a single
/// `<image name=... path=...></image>`. That is not a prompt — recording it as one makes `gist`
/// display a local temp-file path.
fn is_only_attachments(s: &str) -> bool {
    const OPEN: &str = "<image";
    const CLOSE: &str = "</image>";
    let mut rest = s;
    let mut outside = String::new();
    while let Some(i) = rest.find(OPEN) {
        outside.push_str(&rest[..i]);
        rest = match rest[i..].find(CLOSE) {
            Some(j) => &rest[i + j + CLOSE.len()..],
            // Unclosed: treat everything after it as attachment, better conservative.
            None => "",
        };
    }
    outside.push_str(rest);
    outside.trim().is_empty()
}

/// Whether this `role=user` message is a runtime injection rather than something a human typed.
///
/// # Why it exists
///
/// Codex stuffs a great deal into `role=user` before handing it to the model. In that 786 MB
/// session, out of 219 `role=user` messages **only 16 were typed by a human**:
///
/// | count | content |
/// |---|---|
/// | 199 | `<codex_internal_context source="goal">` goal reminder |
/// | 3 | `<environment_context>` environment block |
/// | 1 | `# Files mentioned by the user:` attachment list |
/// | 16 | real user input |
///
/// Not telling them apart costs what not telling a compact summary apart costs: `gist()` picks up
/// `<environment_context>` instead of the first sentence the user actually typed, and
/// `counts().prompts` runs 13 times too high.
///
/// # The test
///
/// Match on a **known prefix**, not on a generalization like "anything starting with `<`" — a
/// user can perfectly well send a message that starts with `<` (pasted HTML, a pasted XML
/// fragment). Better to miss a new form of injection (that only nudges a count high) than to
/// classify real user input as synthetic (that leaves `agit log` unable to tell what this stretch
/// of work was).
///
/// # Why these entries
///
/// A census of 977 `role=user` messages across 108 transcripts on this machine (counts in
/// parentheses). Everything added was checked to be "the runtime's words from start to finish":
///
/// * `<recommended_plugins>` (17) list of uninstalled plugins
/// * `<turn_aborted>` (6) interruption notice, same text as the one under the `developer` role
/// * `<command-name>` (8) / `<local-command-stdout>` (6) the three-part slash command, same shape
///   as Claude Code
/// * `<task-notification>` (1) background task completion notice, likewise
/// * `[Request interrupted by user]` (15) / `...for tool use]` (2) likewise
/// * `The following is the Codex agent history` (41) the evidence prompt of the automatic review
///   sub-agent, with a whole transcript embedded in the body
/// * `# AGENTS.md instructions for <path>` (1) the body of the project convention file; the other
///   spelling is `# AGENTS.md instructions` running straight into a newline and `<INSTRUCTIONS>`
///   (sessions the Codex CLI starts itself use that one, the desktop app uses the one with a
///   path), and both are the runtime's words end to end
/// * `<skill>` (1) the body of SKILL.md
///
/// **Deliberately left out**, because they carry text a human typed after them and adding them
/// would make that text disappear:
///
/// * `<ide_opened_file>` (1) — the body observed there is
///   `<ide_opened_file>...</ide_opened_file>\n\nHi`, and `Hi` is the human's.
/// * `# Selected text:` / `# Response annotations:` (1 each) /
///   `# Context from my IDE setup:` (218) — these go through [`wrapped_human_request`] to be
///   peeled, not through this table. With the last one in this table, the human's actual question
///   would be erased outright across 31 sessions.
///
/// These others **look** like tags but were pasted in by a human, and are exactly why a
/// generalization does not work: `<Trial 352671793 executor_1> ...` (5, a pasted log),
/// `[Einsia][bridge] WS send ...` (4, a pasted log), `[Image #1] ... take a look at ...` (2, an
/// image followed by human words).
fn is_synthetic_user_text(text: &str) -> bool {
    const INJECTED: &[&str] = &[
        "<codex_internal_context",
        "<environment_context>",
        "<user_instructions>",
        "<permissions instructions>",
        "<recommended_plugins>",
        "<turn_aborted>",
        "<skill>",
        // Two headers for the same injection: one with a path, one running straight into a
        // newline. The first is not a prefix of the second, so both are listed.
        "# AGENTS.md instructions for ",
        "# AGENTS.md instructions\n",
        // Without `## My request for Codex:` the message really is nothing but the list.
        "# Files mentioned by the user:",
        // The three-part slash command, same shape in both runtimes.
        "<command-name>",
        "<local-command-stdout>",
        "<local-command-caveat>",
        "<task-notification>",
        // Listed separately: the second does not start with the first.
        "[Request interrupted by user]",
        "[Request interrupted by user for tool use]",
        "The following is the Codex agent history",
    ];
    let s = text.trim_start();
    INJECTED.iter().any(|p| s.starts_with(p))
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

fn extract_content_text(content: Option<&serde_json::Value>) -> Option<String> {
    let arr = content?.as_array()?;
    let parts: Vec<&str> = arr
        .iter()
        .filter_map(|b| b.get("text").and_then(|x| x.as_str()))
        .filter(|t| !t.trim().is_empty())
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// Extract searchable text from Codex's `function_call_output.output` field.
///
/// Current rollouts use a string, while forward-compatible callers may hand us a
/// block array or an object carrying `text`/`content`.  Unknown shapes are left
/// out of the IR and counted as `Other` by the parser.
fn extract_output_text(output: Option<&serde_json::Value>) -> Option<String> {
    let output = output?;
    if let Some(s) = output.as_str() {
        return (!s.trim().is_empty()).then(|| s.to_string());
    }
    if let Some(text) = output.get("text").and_then(|x| x.as_str()) {
        return (!text.trim().is_empty()).then(|| text.to_string());
    }
    if let Some(content) = output.get("content") {
        return extract_content_text(Some(content));
    }
    extract_content_text(Some(output))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_extracted_from_filename() {
        let p = Path::new("rollout-2026-07-25T18-20-01-019f9a81-6fc8-7c41-8280-2d8e6dabe93f.jsonl");
        assert_eq!(
            id_from_filename(p).unwrap(),
            "019f9a81-6fc8-7c41-8280-2d8e6dabe93f"
        );
    }

    #[test]
    fn parses_meta_messages_edits_and_reasoning() {
        let text = r#"{"type":"session_meta","timestamp":"2026-01-01T00:00:00Z","payload":{"id":"s1","cwd":"/repo"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"refactor this"}]}}
{"type":"response_item","payload":{"type":"reasoning","summary":[]}}
{"type":"event_msg","payload":{"type":"patch_apply_end","changes":{"/repo/a.rs":{},"/repo/b.rs":{}},"success":true}}"#;
        let s = Codex.parse(text).unwrap();
        assert_eq!(s.id, "s1");
        assert_eq!(s.cwd.as_deref(), Some("/repo"));
        let c = s.counts();
        assert_eq!(c.prompts, 1);
        assert_eq!(c.edits, 1);
        assert_eq!(c.dropped, 1, "encrypted reasoning counts toward dropped");
    }

    #[test]
    fn codex_function_call_output_is_searchable_output_evidence() {
        let text = concat!(
            r#"{"type":"session_meta","payload":{"id":"s1","cwd":"/repo"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"lr: 2.000e-03"}}"#,
        );
        let s = Codex.parse(text).unwrap();
        assert_eq!(s.counts().outputs, 1);
        assert_eq!(s.events[1].kind, EventKind::ToolResult);
        assert_eq!(s.events[1].text.as_deref(), Some("lr: 2.000e-03"));

        // End-to-end guard: the parsed transcript must satisfy the new output scope,
        // not merely expose an event that no query path can reach.
        let q = crate::domain::query::Query::parse("lr in:output");
        assert!(q.allows(crate::domain::query::EventScope::Output));
        assert!(q.matches_text(s.events[1].text.as_deref().unwrap()));
    }

    #[test]
    fn render_starts_with_session_meta() {
        let s = Session {
            id: "src".into(),
            runtime: "claude-code".into(),
            cwd: None,
            events: vec![Event::text(EventKind::UserPrompt, "q", None)],
        };
        let out = Codex.render(&s, "nid", Path::new("/repo")).unwrap();
        let first: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
        assert_eq!(
            first["type"], "session_meta",
            "the first line must be session_meta"
        );
        assert_eq!(first["payload"]["id"], "nid");
        // Codex only accepts a session_meta whose payload carries a timestamp; the top-level
        // one is the envelope.
        assert!(
            first["payload"]["timestamp"].is_string(),
            "without payload.timestamp Codex refuses to read the line as session_meta"
        );
        assert_eq!(first["timestamp"], first["payload"]["timestamp"]);
        assert_eq!(
            first["payload"]["source"], "cli",
            "an unwritten source makes Codex file the session under the vscode entry point"
        );
        assert_eq!(
            first["payload"]["model_provider"], "openai",
            "openai is hard-coded; a non-openai backend is overridden at launch with -c"
        );
    }

    /// Every message is accompanied by an `event_msg` with the same text: Codex's interface
    /// rebuilds the visible history from event_msg alone, and response_item is only the model's
    /// context. Without the former, the resumed session opens empty.
    #[test]
    fn render_mirrors_every_message_as_a_visible_event() {
        let s = Session {
            id: "src".into(),
            runtime: "claude-code".into(),
            cwd: None,
            events: vec![
                Event::text(EventKind::UserPrompt, "question", None),
                Event::text(EventKind::AssistantReply, "answer", None),
                Event::text(EventKind::UserInterjection, "one more thing", None),
            ],
        };
        let out = Codex.render(&s, "nid", Path::new("/repo")).unwrap();
        let lines: Vec<serde_json::Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let mut seen = 0;
        for w in lines.windows(2) {
            let (item, next) = (&w[0], &w[1]);
            if item["type"] != "response_item" || item["payload"]["type"] != "message" {
                continue;
            }
            seen += 1;
            let text = &item["payload"]["content"][0]["text"];
            let expected = if item["payload"]["role"] == "user" {
                "user_message"
            } else {
                "agent_message"
            };
            assert_eq!(
                next["type"], "event_msg",
                "a message is followed by its own event_msg"
            );
            assert_eq!(next["payload"]["type"], expected);
            assert_eq!(
                &next["payload"]["message"], text,
                "both bodies are word for word equal"
            );
        }
        assert_eq!(seen, 3, "all three messages are checked");
    }

    /// When enrich supplies details, `function_call` carries the real arguments and the paired
    /// output is the real output.
    #[test]
    fn render_with_details_fills_arguments_and_output() {
        let s = Session {
            id: "s".into(),
            runtime: "claude-code".into(),
            cwd: None,
            events: vec![
                Event::text(EventKind::UserPrompt, "run it", None),
                Event {
                    kind: EventKind::ToolUse,
                    text: Some("Bash".into()),
                    timestamp: None,
                    paths: vec![],
                    tool: Some("Bash".into()),
                    line: None,
                },
            ],
        };
        let mut details = crate::adapter::ToolDetails::default();
        details.insert(
            1,
            crate::adapter::ToolDetail {
                input: Some(serde_json::json!({"command": "ls"})),
                output: Some("total 0".into()),
                error: false,
            },
        );
        let out = Codex
            .render_with(&s, "nid", Path::new("/r"), &details)
            .unwrap();
        let lines: Vec<serde_json::Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let call = lines
            .iter()
            .find(|v| v["payload"]["type"] == "function_call")
            .expect("the call is rendered");
        assert_eq!(
            call["payload"]["arguments"], "{\"command\":\"ls\"}",
            "arguments are a JSON-encoded string (the Codex convention)"
        );
        let output = lines
            .iter()
            .find(|v| v["payload"]["type"] == "function_call_output")
            .expect("the output is rendered");
        assert_eq!(output["payload"]["output"], "total 0");
        assert_eq!(output["payload"]["call_id"], call["payload"]["call_id"]);
    }

    /// A cross-runtime round trip: the prompt and reply counts are conserved.
    ///
    /// This is the core correctness guarantee of cross-runtime resume — `agit resume --as` rests
    /// on it.
    #[test]
    fn roundtrip_preserves_prompts_and_replies() {
        let text = r#"{"type":"session_meta","payload":{"id":"s","cwd":"/r"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"question"}]}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}}"#;
        let ir1 = Codex.parse(text).unwrap();
        let cc = super::super::claude_code::ClaudeCode;
        let rendered = cc.render(&ir1, "new", Path::new("/r")).unwrap();
        let ir2 = cc.parse(&rendered).unwrap();
        assert_eq!(ir1.counts().prompts, ir2.counts().prompts);
        assert_eq!(ir1.counts().replies, ir2.counts().replies);
        // The content lines up too, not only the counts.
        assert_eq!(ir1.gist(50), ir2.gist(50));
    }
    /// A text that opens at a compact boundary (a branch VIEW's resume view): replacement_history
    /// is the only carrier of the retained context and must expand into events — without that,
    /// the resumed session has not even an opening prompt. A record carrying the extra `ordinal`
    /// field is recognized just the same.
    #[test]
    fn an_opening_compacted_record_expands_its_replacement_history() {
        let text = r#"{"type":"compacted","ordinal":3350,"timestamp":"2026-08-31T00:00:00Z","payload":{"window_number":4,"first_window_id":"w1","replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"open a new MR off main"}]},{"type":"message","role":"assistant","content":[{"type":"output_text","text":"sure, handled in the same MR"}]},{"type":"compaction"}]}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"a later reply"}]}}"#;
        let s = Codex.parse(text).unwrap();
        let c = s.counts();
        assert_eq!(
            c.prompts, 1,
            "the opening prompt expands out of replacement_history"
        );
        assert_eq!(
            c.replies, 2,
            "the retained reply plus the reply after the boundary"
        );
        assert_eq!(c.compactions, 1, "the boundary itself is still there");
        assert_eq!(
            s.gist(30).as_deref(),
            Some("open a new MR off main"),
            "gist recovers the real prompt"
        );
    }

    /// A second compact boundary in the same transcript does not expand: the messages the first
    /// one expanded are already in the IR, and expanding again is a duplicate hit.
    #[test]
    fn only_the_opening_compacted_record_expands() {
        let text = r#"{"type":"compacted","payload":{"window_number":1,"replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"the opening"}]}]}}
{"type":"compacted","payload":{"window_number":2,"replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"the opening"}]}]}}"#;
        let s = Codex.parse(text).unwrap();
        assert_eq!(
            s.counts().prompts,
            1,
            "the second boundary does not expand again"
        );
        assert_eq!(s.counts().compactions, 2);
    }

    /// Codex's compact payload lives in the **top-level** `compacted` record, not in an event_msg
    /// subtype.
    ///
    /// This pins that the payload is read from that record: an implementation that recognizes only
    /// the `event_msg/context_compacted` notice following it (payload carries a type and no body)
    /// misses the whole mechanism and escapes every count.
    #[test]
    fn recognizes_top_level_compacted_record() {
        let text = r#"{"type":"session_meta","payload":{"id":"s1","cwd":"/r"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"the real opening prompt"}]}}
{"type":"compacted","timestamp":"2026-07-17T18:11:00Z","payload":{"message":"","window_number":1,"window_id":"w2","previous_window_id":"w1","first_window_id":"w1","replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"the real opening prompt"}]},{"type":"compaction"}]}}
{"type":"event_msg","payload":{"type":"context_compacted"}}"#;
        let s = Codex.parse(text).unwrap();
        let c = s.counts();

        assert_eq!(c.compactions, 1, "a compacted record is recognized");
        assert_eq!(c.prompts, 1, "a compact must not count as a user prompt");
        assert_eq!(
            s.gist(30).as_deref(),
            Some("the real opening prompt"),
            "gist takes the real prompt, untouched by the compact"
        );
    }

    /// Codex's compact is a **filter** (lossless), not a summary (lossy).
    ///
    /// The distinction decides whether merge can take it as input: verbatim original text can, a
    /// summary cannot.
    #[test]
    fn codex_compact_is_filtered_not_summarized() {
        let text = r#"{"type":"compacted","payload":{"window_number":3,"replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"x"}]}]}}"#;
        let s = Codex.parse(text).unwrap();
        let e = &s.events[0];
        assert_eq!(e.kind, EventKind::CompactFiltered);
        assert!(
            !e.kind.is_lossy_summary(),
            "Codex's replacement_history is verbatim original text, usable by merge as is"
        );
        assert!(e.kind.is_compact());
        // The window number lands in the text, which helps diagnosis (it says how often the
        // session ran into the context limit).
        assert!(
            e.text.as_deref().unwrap_or("").contains("#3"),
            "the window number must be visible: {:?}",
            e.text
        );
    }

    /// The body of replacement_history does not enter the IR.
    ///
    /// Those messages already appeared verbatim in the transcript before this record; putting them
    /// in again multiplies duplicate hits in both counts and search (a sentence compacted 11 times
    /// would be counted 12 times).
    #[test]
    fn replacement_history_body_is_not_duplicated_into_ir() {
        let text = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"a one-of-a-kind original line"}]}}
{"type":"compacted","payload":{"window_number":1,"replacement_history":[{"type":"message","role":"user","content":[{"type":"input_text","text":"a one-of-a-kind original line"}]}]}}"#;
        let s = Codex.parse(text).unwrap();
        let hits = s
            .events
            .iter()
            .filter(|e| {
                e.text
                    .as_deref()
                    .unwrap_or("")
                    .contains("a one-of-a-kind original line")
            })
            .count();
        assert_eq!(
            hits, 1,
            "the original line appears once in the IR, never copied by a compact"
        );
        assert_eq!(s.counts().prompts, 1);
    }

    /// An unknown response_item payload type and an unknown top-level record type both enter the
    /// dropped total, never silently.
    #[test]
    fn unknown_record_types_are_counted_not_silently_dropped() {
        let text = concat!(
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/r"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"prompt"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"web_search_call","status":"completed"}}"#,
            "\n",
            r#"{"type":"world_state","payload":{}}"#,
            "\n",
        );
        let s = Codex.parse(text).unwrap();
        let c = s.counts();
        assert_eq!(c.prompts, 1);
        assert_eq!(
            c.dropped, 2,
            "an unknown payload type and an unknown top-level type are both counted: {:?}",
            s.events
        );
        let others: Vec<usize> = s
            .events
            .iter()
            .filter(|e| e.kind == EventKind::Other)
            .filter_map(|e| e.line)
            .collect();
        assert_eq!(
            others,
            vec![2, 3],
            "the line number comes along, to address the source"
        );
    }

    /// `custom_tool_call` is a tool call, in the same class as `function_call`: it is evidence of
    /// what the agent did, and whether it got its output decides whether the turn has ended.
    #[test]
    fn custom_tool_calls_are_tool_use_and_pair_by_call_id() {
        let text = concat!(
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/r"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"prompt"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"c1","name":"exec_command","input":"agit commit"}}"#,
            "\n",
        );
        let s = Codex.parse(text).unwrap();
        assert_eq!(s.events[1].kind, EventKind::ToolUse);
        assert_eq!(s.events[1].tool.as_deref(), Some("exec_command"));

        let open = Codex.open_tool_calls(text);
        assert_eq!(
            open,
            vec![OpenCall {
                line: 2,
                call_id: "c1".into(),
                record: "custom_tool_call".into(),
                name: "exec_command".into(),
            }]
        );

        let closed = format!(
            "{text}{}\n",
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c1","output":"ok"}}"#
        );
        assert!(Codex.open_tool_calls(&closed).is_empty());
        let s = Codex.parse(&closed).unwrap();
        assert_eq!(s.events[2].kind, EventKind::ToolResult);
    }

    /// Only `call_id` pairs; when an output's id does not match, that call stays open, and a call
    /// with no id is neither open nor closed.
    #[test]
    fn open_calls_match_strictly_by_call_id() {
        let text = concat!(
            r#"{"type":"response_item","payload":{"type":"function_call","call_id":"a","name":"shell","arguments":"{}"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"b","output":"?"}}"#,
            "\n",
        );
        let open = Codex.open_tool_calls(text);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].call_id, "a");
        assert_eq!(open[0].line, 0);
    }

    /// A call replayed across runtimes is closed: every `function_call` is followed by a
    /// `function_call_output` with the same `call_id`, or Codex refuses to resume.
    #[test]
    fn rendered_tool_calls_are_paired_with_an_output() {
        let s = Session {
            id: "x".into(),
            runtime: "claude-code".into(),
            cwd: None,
            events: vec![
                Event::text(EventKind::UserPrompt, "run it", None),
                Event {
                    kind: EventKind::ToolUse,
                    text: Some("Bash".into()),
                    timestamp: None,
                    paths: vec![],
                    tool: Some("Bash".into()),
                    line: None,
                },
                Event::text(EventKind::ToolResult, "output", None),
                Event::text(EventKind::AssistantReply, "the run finished", None),
            ],
        };
        let out = Codex.render(&s, "id", Path::new("/r")).unwrap();
        assert!(Codex.open_tool_calls(&out).is_empty(), "{out}");
        let lines: Vec<serde_json::Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let at = lines
            .iter()
            .position(|l| l["payload"]["type"] == "function_call")
            .expect("the call is rendered");
        let call = &lines[at]["payload"];
        let output = &lines[at + 1]["payload"];
        assert_eq!(output["type"], "function_call_output");
        assert_eq!(call["call_id"], output["call_id"]);
        assert_eq!(output["output"], CROSS_RUNTIME_OUTPUT_PLACEHOLDER);
        // The source-side output is not attributed to this call, and must not leak out as a user
        // message either.
        assert!(!out.contains("\"output\"\n") && !out.contains("input_text\",\"text\":\"output\""));
    }

    /// compact boundaries are emitted: a summary is the legitimate starting context of a resumed
    /// session, and a filtered boundary carries a label saying it is not conversation content (the
    /// filtering decision belongs to the session/VIEW built at commit time).
    #[test]
    fn compact_boundaries_are_rendered_as_resume_context() {
        let s = Session {
            id: "x".into(),
            runtime: "codex".into(),
            cwd: None,
            events: vec![
                Event::text(
                    EventKind::CompactSummary,
                    "the compacted context summary",
                    None,
                ),
                Event::text(
                    EventKind::CompactFiltered,
                    "context window #1, 9 messages kept",
                    None,
                ),
                Event::text(EventKind::UserPrompt, "prompt", None),
            ],
        };
        let out = Codex.render(&s, "nid", Path::new("/r")).unwrap();
        let messages: Vec<serde_json::Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .filter(|v: &serde_json::Value| v["payload"]["type"] == "message")
            .collect();
        // Summary + labelled filtered boundary + prompt.
        assert_eq!(messages.len(), 3, "three messages: {out}");
        assert!(
            out.contains("the compacted context summary"),
            "the summary must be there"
        );
        assert!(
            out.contains("(this context was compact-filtered by the source runtime: context window #1, 9 messages kept)"),
            "a filtered boundary must carry its label: {out}"
        );
        // Both kinds of compact boundary go out as user/input_text.
        for v in &messages[..2] {
            assert_eq!(v["payload"]["role"], "user");
            assert_eq!(v["payload"]["content"][0]["type"], "input_text");
        }
    }

    /// Codex stuffs a great deal of runtime injection into `role=user`; none of it is a prompt.
    ///
    /// This pins the split: in that 786MB session, out of 219 role=user messages only 16 were
    /// typed by a human. Without the split, gist picks up `<environment_context>` and prompts runs
    /// 13 times too high.
    #[test]
    fn synthetic_user_injections_are_not_prompts() {
        // The order follows a real file: the environment block sits **before** the first real
        // prompt.
        let text = r##"{"type":"session_meta","payload":{"id":"s","cwd":"/r"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n  <cwd>/r</cwd>\n</environment_context>"}]}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions\n\n<INSTRUCTIONS>\n# Global rules\nbe nice\n</INSTRUCTIONS>"}]}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"look at this project"}]}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<codex_internal_context source=\"goal\">\nContinue working\n</codex_internal_context>"}]}}"##;
        let s = Codex.parse(text).unwrap();
        let c = s.counts();

        assert_eq!(
            c.prompts, 1,
            "only one is typed by a human: got {}",
            c.prompts
        );
        assert_eq!(
            s.gist(20).as_deref(),
            Some("look at this project"),
            "gist skips the environment block and takes the real prompt"
        );
        assert_eq!(c.dropped, 3, "the three injections are recorded as dropped");
    }

    /// `role=developer` is the runtime instructing the model, not the model speaking.
    ///
    /// This pins the allowlist: classifying by `role != "user"` renders
    /// `<permissions instructions>` (the sandbox description) as prose the agent produced. Not one
    /// of the 224 `developer` messages on this machine is a human or the model speaking.
    #[test]
    fn developer_role_is_runtime_speech_not_an_assistant_reply() {
        let text = concat!(
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<permissions instructions>\nFilesystem sandboxing defines which files can be read"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"<collaboration_mode># Collaboration Mode: Default"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"this line is the model speaking"}]}}"#,
        );
        let s = Codex.parse(text).unwrap();
        let kinds: Vec<EventKind> = s.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::Other,
                EventKind::Other,
                EventKind::AssistantReply
            ],
            "only assistant is the model speaking: {kinds:?}"
        );
        let c = s.counts();
        assert_eq!(
            c.replies, 1,
            "the two injections must not count toward \"N replies\""
        );
        assert_eq!(c.dropped, 2);
        // The body stays, so the page can still mark it as a runtime injection instead of having
        // it vanish out of nowhere.
        assert!(s.events[0].text.as_deref().unwrap().contains("sandboxing"));
    }

    /// An unknown role is always `Other`, never guessed to be a reply.
    ///
    /// An allowlist and not a denylist: when Codex introduces a new role, the worst outcome is
    /// that it lands in dropped, not that it is printed as the agent's words all over again.
    #[test]
    fn an_unknown_role_is_dropped_rather_than_attributed_to_the_model() {
        let text = r#"{"type":"response_item","payload":{"type":"message","role":"orchestrator","content":[{"type":"input_text","text":"some injection that only exists later"}]}}"#;
        let s = Codex.parse(text).unwrap();
        assert_eq!(s.events[0].kind, EventKind::Other);
        assert_eq!(s.counts().replies, 0);
    }

    /// The text a human typed inside an attachment wrapper is peeled out, never erased whole.
    ///
    /// This pins the peeling: with `# Files mentioned by the user:` in the injected-prefix table,
    /// out of 76 such messages on this machine the human's own text vanished in 73 — not a prompt,
    /// not a turn boundary, not usable as a gist. That is exactly the direction injection
    /// detection must avoid.
    #[test]
    fn a_human_request_wrapped_in_an_attachment_list_is_still_a_prompt() {
        let text = concat!(
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/r"}}"#,
            "\n",
            r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# Files mentioned by the user:\n\n## clip.png: /tmp/clip.png\n\n## My request for Codex:\nmake me a table of these checkpoint scores\n\n<image name=[Image #1] path=\"/tmp/clip.png\">\n</image>"}]}}"##,
        );
        let s = Codex.parse(text).unwrap();
        assert_eq!(
            s.counts().prompts,
            1,
            "the text a human typed counts as one prompt"
        );
        assert_eq!(
            s.gist(14).as_deref(),
            Some("make me a tabl…"),
            "gist takes the span after the marker line, not the runtime-generated file list"
        );
    }

    /// Editor state injected by the IDE is peeled off, leaving only the human's actual question.
    ///
    /// # This pins a mislabeling bug, not just a count
    ///
    /// The header runs 291 to 2909 characters (current file, open tabs, selected lines) while the
    /// human's actual question is a few dozen. Unpeeled, one person asking two questions in a row
    /// about the same file produces two messages whose bigram Jaccard lands between 0.90 and 1.00,
    /// so `outcome.rs` reads "the same question was asked N times over" as `Failed`.
    ///
    /// Across the corpus, 218 such messages spread over 31 sessions; auditing 20 `failed` labels,
    /// three trace back to this one (oc-19 / oc-28 / oc-33 in `eval/outcomes_to_label.jsonl`),
    /// while none of those 31 sessions was actually stuck.
    #[test]
    fn ide_context_is_peeled_so_only_the_human_question_remains() {
        let one = |q: &str| {
            format!(
                concat!(
                    r#"{{"type":"session_meta","payload":{{"id":"s","cwd":"/r"}}}}"#,
                    "\n",
                    r##"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"# Context from my IDE setup:\n## Active file: src/thing.py\n## Open tabs:\n- a.py: src/a.py\n- b.py: src/b.py\n\n## My request for Codex:\n{}"}}]}}}}"##,
                ),
                q
            )
        };

        let s = Codex.parse(&one("why return None here")).unwrap();
        assert_eq!(
            s.counts().prompts,
            1,
            "the text a human typed counts as one prompt"
        );
        assert_eq!(
            s.gist(20).as_deref(),
            Some("why return None here"),
            "gist is the human's question, not the editor state"
        );

        // The key property: **two different questions must not become similar by sharing that
        // header**.
        let a = Codex.parse(&one("why return None here")).unwrap();
        let b = Codex.parse(&one("vectorize this loop for me")).unwrap();
        let ga = crate::domain::text::bigrams(a.gist(200).unwrap().as_str());
        let gb = crate::domain::text::bigrams(b.gist(200).unwrap().as_str());
        let sim = crate::domain::text::similarity(&ga, &gb);
        assert!(
            sim < 0.5,
            "with the header peeled, two different questions must not look alike: got {sim:.3}"
        );
    }

    /// An image dropped in with nothing typed → not a prompt.
    #[test]
    fn an_attachment_only_message_is_not_a_prompt() {
        let text = r##"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# Files mentioned by the user:\n\n## a.png: /tmp/a.png\n\n## My request for Codex:\n\n\n<image name=[Image #1] path=\"/tmp/a.png\">\n</image>"}]}}"##;
        let s = Codex.parse(text).unwrap();
        assert_eq!(s.events[0].kind, EventKind::Other);
        assert_eq!(
            s.counts().prompts,
            0,
            "recording it as a prompt makes gist display a temp-file path"
        );
    }

    /// Peeling happens only when the whole message starts with a known wrapper.
    #[test]
    fn the_request_marker_only_peels_inside_a_known_wrapper() {
        // A human quoting the sentence themselves — it must not be cut at the marker line.
        let whole = "who added this `## My request for Codex:` marker line?";
        assert!(wrapped_human_request(whole).is_none());
        // A wrapper with no marker line → falls back to the prefix table, and the message is
        // still an injection.
        assert!(
            wrapped_human_request("# Files mentioned by the user:\n\n## a.png: /tmp/a.png")
                .is_none()
        );
        assert!(is_synthetic_user_text(
            "# Files mentioned by the user:\n\n## a.png: /tmp/a.png"
        ));
    }

    /// The test is a known prefix, not "starts with <".
    #[test]
    fn user_authored_angle_bracket_text_is_still_a_prompt() {
        // A user can perfectly well paste an HTML/XML fragment. Classifying it as synthetic
        // leaves agit log unable to tell what this stretch of work was — far worse than a count
        // running high.
        assert!(is_synthetic_user_text("<environment_context>\n x"));
        assert!(is_synthetic_user_text(
            "  <codex_internal_context source=\"goal\">"
        ));
        assert!(is_synthetic_user_text(
            "# Files mentioned by the user:\nfoo"
        ));

        assert!(!is_synthetic_user_text(
            "<div>rewrite this HTML for me</div>"
        ));
        assert!(!is_synthetic_user_text(
            "<T> how is this generic parameter written"
        ));
        assert!(!is_synthetic_user_text("an ordinary prose prompt"));
        assert!(!is_synthetic_user_text(""));
    }

    /// The forms added to the table after the census of 977 `role=user` messages on this machine,
    /// and the ones **deliberately left out**.
    ///
    /// The counterexample column matters more than the positive one: it shows why a generalization
    /// like "starts with `<` / `[`" does not work — most of them are text a user pasted in.
    #[test]
    fn the_injected_list_covers_what_the_census_found() {
        for s in [
            "<recommended_plugins>\nHere is a list of plugins",
            "<turn_aborted>\nThe user interrupted the previous turn on purpose.",
            "<command-name>/effort</command-name>",
            "<local-command-stdout>Set effort level to xhigh",
            "<task-notification>\n<task-id>bqou112pz</task-id>",
            "[Request interrupted by user]",
            "[Request interrupted by user for tool use]",
            "The following is the Codex agent history added since your last review",
            "# AGENTS.md instructions for /Users/nana/Projects/x\n\n<INSTRUCTIONS>",
            // A session the Codex CLI starts carries no path; the header runs straight into a
            // newline and then the body.
            "# AGENTS.md instructions\n\n<INSTRUCTIONS>\n# Global rules\nbe nice\n</INSTRUCTIONS>",
            "<skill>\n<name>browser:control-in-app-browser</name>",
        ] {
            assert!(is_synthetic_user_text(s), "must be an injection: {s}");
        }

        for s in [
            // A human naming this file in a prompt is not an injection: what follows the header
            // is neither a path nor a newline.
            "# AGENTS.md instructions could use a rewrite, right?",
            // An injection block followed by text a human typed. Calling the whole message an
            // injection makes `Hi` disappear.
            "<ide_opened_file>The user opened the file /x in the IDE.</ide_opened_file>\n\nHi",
            // A log the user pasted, not a tag.
            "<Trial 352671793 executor_1> wrapped-autoresearch [agent/trial]",
            "[Einsia][bridge] WS send 69270c43 5:90001+::",
            // An image followed by real words.
            "[Image #1] [Image #2] show me every command that pushes onto the message queue",
            // Goes down the peeling path; the whole message must not be called an injection.
            "# Selected text:\n\n## Selection 1\nx\n\n## My request for Codex:\nmake it like this",
        ] {
            assert!(!is_synthetic_user_text(s), "must not be an injection: {s}");
        }
    }

    /// `task_complete` is recognized as a `TurnEnd`.
    ///
    /// It does not affect the version (a version is the snapshot at commit time and needs no
    /// termination signal), but it is a signal that genuinely exists in Codex's data: render skips
    /// it, and activity counts must not treat it as a reply. Reading it wrong inflates summaries
    /// like "N replies".
    #[test]
    fn task_complete_becomes_a_turn_end_event() {
        let text = concat!(
            r#"{"type":"session_meta","payload":{"id":"AB","cwd":"/w"}}"#,
            "\n",
            r#"{"type":"response_item","timestamp":"t1","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"do something"}]}}"#,
            "\n",
            r#"{"type":"event_msg","timestamp":"t2","payload":{"type":"task_complete","turn_id":"x","last_agent_message":"done"}}"#,
            "\n",
        );
        let s = Codex.parse(text).unwrap();
        assert_eq!(
            s.events
                .iter()
                .filter(|e| e.kind == EventKind::TurnEnd)
                .count(),
            1,
            "task_complete produces one TurnEnd"
        );
        // It counts toward no activity total.
        let c = s.counts();
        assert_eq!(c.prompts, 1);
        assert_eq!(c.replies, 0);
        assert_eq!(c.dropped, 0, "a TurnEnd is not a dropped event");

        // End to end: it changes neither the turn count nor that turn's hash.
        let chain = crate::domain::turn::chain_of(&s);
        assert_eq!(chain.len(), 1, "a marker does not form a turn of its own");
    }
}
