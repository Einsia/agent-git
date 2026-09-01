//! The Cursor adapter. **Read-only**.
//!
//! # On-disk format
//!
//! ```text
//! main session  ~/.cursor/projects/<slug>/agent-transcripts/<uuid>/<uuid>.jsonl
//! subagent      ~/.cursor/projects/<slug>/agent-transcripts/<parent>/subagents/<sub>.jsonl
//! ```
//!
//! **One directory per session, with the jsonl inside it carrying the same name** — not
//! `agent-transcripts/<uuid>.jsonl`. It is the easiest thing to get wrong here.
//!
//! `slug` differs from the Claude Code one by a single leading character (see [`slug_for`]).
//!
//! A line takes one of two shapes only (observed over 104 files, 5335 records):
//! `{role, message}` and `{type:"turn_ended", status}`. Blocks are only `text` and `tool_use`.
//!
//! # Four absences decide what this adapter can do
//!
//! The transcript has **no** `tool_result` (not one line of tool output reaches disk), **no**
//! thinking, **no** timestamp field, **no** `sessionId` / `cwd` / model.
//!
//! The first two are genuinely lost (the truth lives in `state.vscdb`, see below). The other two
//! have substitutes: time comes from the `<timestamp>` tag in the user body, and the id and cwd
//! can only be recovered from the **path** — so this adapter has to implement
//! [`Adapter::parse_at`]; `parse(&str)` alone cannot reach them.
//!
//! # Why it is not writable
//!
//! The transcript is a **projection**, not the source of truth: one session's transcript runs 75
//! lines against 154 bubbles in `state.vscdb`, losing tool results, thinking, checkpoints, diffs
//! and token counts. The truth (`composerData.conversationState`) is opaque base64 protobuf
//! behind a `~` prefix, with a `blobEncryptionKey` beside it; and this machine has no resumable
//! CLI entry point either (`/usr/local/bin/cursor` is a VS Code style launcher).
//!
//! The worst failure shape is "the command returns success and the user opens Cursor to
//! nothing", so [`Adapter::capability`] declares [`Capability::ImportOnly`] and
//! [`Adapter::install`] errors outright — refusing **before any work starts** rather than
//! finding out once the files are written.
//!
//! # Compaction is invisible here
//!
//! `counts().compactions` on the Cursor side is **always 0, and that is not a bug**. The four
//! sessions with the highest `contextUsagePercent` were each checked against their transcripts:
//! the shape is exactly that of an ordinary session. `state.vscdb` carries fields such as
//! `speculativeSummarizationEncryptionKey`, so compaction output is encrypted into the database
//! and leaves no trace in the transcript.

use super::{Adapter, Capability, Event, EventKind, Installed, Session, SessionRef};
use crate::Result;
use anyhow::{Context, bail};
use std::path::{Path, PathBuf};

pub struct Cursor;

/// Name of the directory holding the session directories. The only stable anchor in the path
/// template, and what recovering the slug keys off.
const TRANSCRIPTS: &str = "agent-transcripts";

fn projects_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("$HOME is not set")?;
    Ok(PathBuf::from(home).join(".cursor").join("projects"))
}

/// Maps a cwd to Cursor's project directory name.
///
/// The Claude Code slug with the leading `-` removed, in every sample observed:
///
/// ```text
/// /Users/nana/Projects/AgentGit
///   Cursor        Users-nana-Projects-AgentGit
///   Claude Code  -Users-nana-Projects-AgentGit
/// ```
pub fn slug_for(cwd: &Path) -> String {
    crate::domain::store::slug_for(cwd)
        .trim_start_matches('-')
        .to_string()
}

impl Adapter for Cursor {
    fn id(&self) -> &'static str {
        "cursor"
    }

    fn cli(&self) -> &'static str {
        "cursor"
    }

    /// Cursor is import-only: the transcript is a projection and cannot be installed back
    /// (module docs, "Why it is not writable").
    fn capability(&self) -> Capability {
        Capability::ImportOnly
    }

    fn format(&self) -> &'static str {
        "cursor"
    }

    fn sessions_for(&self, repo: &Path) -> Result<Vec<SessionRef>> {
        let dir = projects_dir()?.join(slug_for(repo)).join(TRANSCRIPTS);
        if !dir.is_dir() {
            // This project has never run under Cursor — a normal state, not an error.
            return Ok(vec![]);
        }
        Ok(collect_project(
            &dir,
            Some(repo.to_string_lossy().to_string()),
        ))
    }

    /// Lists every session.
    ///
    /// **Do not pretend the cwd can be recovered**: three kinds of slug are not paths at all —
    /// `empty-window` (no folder opened), a bare numeric workspace id, and `var-folders-...` (a
    /// temporary directory). So this fills None uniformly, matching Claude Code, and a caller
    /// that needs the cwd reads the transcript instead ([`Adapter::parse_at`] recovers it from
    /// the absolute paths in the body).
    fn all_sessions(&self) -> Result<Vec<SessionRef>> {
        let root = projects_dir()?;
        if !root.is_dir() {
            return Ok(vec![]);
        }
        let mut out = vec![];
        for e in std::fs::read_dir(&root)?.flatten() {
            let dir = e.path().join(TRANSCRIPTS);
            if dir.is_dir() {
                out.extend(collect_project(&dir, None));
            }
        }
        Ok(out)
    }

    /// Reverse lookup: with a cwd it goes straight to the file, otherwise it scans every
    /// project.
    ///
    /// **One session id can appear under two slugs with diverging content** (the same id runs 75
    /// lines under `Users-nana-Projects-AgentGit` and 56 lines under `empty-window` — not a
    /// copy). The cause is unknown, so the fallback takes the newest mtime — a guess, but better
    /// than taking whatever readdir happens to return first, which is random.
    ///
    /// No index database: Cursor already splits by project on disk, an order of magnitude away
    /// from the flat directory Codex has to scan whole.
    fn resolve(&self, session_id: &str, cwd: Option<&Path>) -> Option<PathBuf> {
        let root = projects_dir().ok()?;

        if let Some(c) = cwd {
            let base = root.join(slug_for(c)).join(TRANSCRIPTS);
            if let Some(p) = in_project(&base, session_id) {
                return Some(p);
            }
        }

        let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
        for e in std::fs::read_dir(&root).ok()?.flatten() {
            let Some(p) = in_project(&e.path().join(TRANSCRIPTS), session_id) else {
                continue;
            };
            let m = mtime_of(&p);
            if best.as_ref().is_none_or(|(bm, _)| m > *bm) {
                best = Some((m, p));
            }
        }
        best.map(|(_, p)| p)
    }

    /// Parses from the **path**, which is the only way to reach the id and the cwd.
    ///
    /// The transcript carries neither, so a bare `parse(&str)` can never produce them — which is
    /// exactly why the trait has `parse_at`.
    fn parse_at(&self, path: &Path) -> Result<Session> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let (mut s, hints) = parse_text(&text);
        s.id = path
            .file_stem()
            .and_then(|x| x.to_str())
            .unwrap_or_default()
            .to_string();
        s.cwd = cwd_from(path, &hints);
        Ok(s)
    }

    /// Bare text parse. **The id and the cwd come back empty**, because the transcript has no
    /// such fields. A caller that needs them uses [`Adapter::parse_at`].
    fn parse(&self, text: &str) -> Result<Session> {
        Ok(parse_text(text).0)
    }

    /// Renders into the Cursor form.
    ///
    /// **Nothing consumes this**: `install` refuses, so the path from the intermediate
    /// representation (IR) to Cursor leads nowhere. It stays because the trait requires it, and
    /// because an honest implementation is worth more than `unimplemented!()` — it writes down
    /// what the IR-to-Cursor mapping looks like.
    fn render(&self, session: &Session, _new_id: &str, _cwd: &Path) -> Result<String> {
        let mut out = String::new();
        for e in &session.events {
            let block = match e.kind {
                EventKind::UserPrompt | EventKind::UserInterjection | EventKind::AssistantReply => {
                    let Some(t) = e.text.as_deref().filter(|t| !t.trim().is_empty()) else {
                        continue;
                    };
                    serde_json::json!({
                        "role": if e.kind == EventKind::AssistantReply { "assistant" } else { "user" },
                        "message": { "content": [{ "type": "text", "text": t }] }
                    })
                }
                EventKind::ToolUse | EventKind::FileEdit => serde_json::json!({
                    "role": "assistant",
                    "message": { "content": [{
                        "type": "tool_use",
                        "name": e.tool.clone().unwrap_or_else(|| "tool".into()),
                        // The IR keeps no tool arguments, only paths.
                        "input": { "path": e.paths.first().cloned().unwrap_or_default() }
                    }]}
                }),
                // Same reasoning as the other two adapters: a compact boundary is the source
                // runtime's context-management trace rather than conversation content, a
                // turn-end marker is an internal signal, and tool output has no paired call
                // id. None of them is re-emitted.
                EventKind::CompactFiltered
                | EventKind::CompactSummary
                | EventKind::TurnEnd
                | EventKind::ToolResult
                | EventKind::Other => continue,
            };
            out.push_str(&serde_json::to_string(&block)?);
            out.push('\n');
        }
        Ok(out)
    }

    fn mint_id(&self) -> String {
        uuid::Uuid::now_v7().to_string()
    }

    fn install(&self, _content: &str, _new_id: &str, _cwd: &Path) -> Result<Installed> {
        // The layer above refuses on `capability()` first; reaching here means someone went
        // around that gate. This blocks it again: better one redundant check than the failure
        // where "the command returns success and the user opens Cursor to nothing".
        bail!(
            "Cursor sessions cannot be installed back.\n  \
             Its `agent-transcripts/*.jsonl` is a projection — the truth lives in\n  \
             encrypted protobuf inside state.vscdb, with no resumable CLI entry.\n  \
             Use `--as claude-code` or `--as codex` instead; Cursor as a **source** is complete."
        )
    }

    /// `cursor` on PATH does not mean a session can be resumed (it is a VS Code style launcher).
    ///
    /// Reporting false keeps commands like `resume` from treating it as a candidate. The real
    /// gate is [`Adapter::capability`]; this only declines to tell a second lie.
    fn available(&self) -> bool {
        false
    }
}

/// Every transcript under one project directory (`<slug>/agent-transcripts`).
///
/// Main sessions and subagents both count. A subagent is a self-contained stretch of work, and
/// hiding it only makes `agit import <prefix>` and `doctor` blind to it.
fn collect_project(dir: &Path, cwd: Option<String>) -> Vec<SessionRef> {
    let mut out = vec![];
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let d = e.path();
        let Some(name) = d.file_name().and_then(|x| x.to_str()).map(String::from) else {
            continue;
        };
        if !d.is_dir() {
            continue;
        }
        let main = d.join(format!("{name}.jsonl"));
        if main.is_file() {
            out.push(session_ref(name, main, cwd.clone()));
        }
        let subs = d.join("subagents");
        let Ok(sub_entries) = std::fs::read_dir(&subs) else {
            continue;
        };
        for s in sub_entries.flatten() {
            let p = s.path();
            if p.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = p.file_stem().and_then(|x| x.to_str()).map(String::from) else {
                continue;
            };
            out.push(session_ref(id, p, cwd.clone()));
        }
    }
    out
}

fn session_ref(id: String, path: PathBuf, cwd: Option<String>) -> SessionRef {
    SessionRef {
        id,
        mtime: mtime_of(&path),
        path,
        runtime: "cursor",
        cwd,
        gist: None,
    }
}

fn mtime_of(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

/// Finds an id under one project's `agent-transcripts`: main session first, then subagents.
fn in_project(base: &Path, id: &str) -> Option<PathBuf> {
    let main = base.join(id).join(format!("{id}.jsonl"));
    if main.is_file() {
        return Some(main);
    }
    // A subagent's parent id is unknown, so this scans one level.
    for e in std::fs::read_dir(base).ok()?.flatten() {
        let p = e.path().join("subagents").join(format!("{id}.jsonl"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Parses the body, bringing out the absolute-path candidates that recovering the cwd needs.
///
/// Candidates and events are collected in **one pass**: a transcript can run to megabytes, and a
/// second scan just for the cwd does not pay for itself.
fn parse_text(text: &str) -> (Session, Vec<String>) {
    let mut events = vec![];
    // See [`cwd_from`].
    let mut hints: Vec<String> = vec![];

    for (lineno, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // A bad line is skipped rather than failing the whole parse (as in the other two
        // adapters): a transcript can be truncated, and an incomplete session still has
        // content worth keeping.
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        // ── Turn end ──
        //
        // Added in Cursor 3.13.x (2026-07-30), so most transcripts carry none of these.
        // **It must be treated as an optional field**: this kind of difference along the
        // time axis within one host is more dangerous than a cross-host one, because moving
        // to another machine to test never exposes it.
        if v.get("type").and_then(|x| x.as_str()) == Some("turn_ended") {
            let status = v.get("status").and_then(|x| x.as_str()).unwrap_or("");
            // A failure is still a TurnEnd — `error` / `aborted` both mean "this turn grows
            // no further". The interruption itself has diagnostic value the IR cannot
            // express, so it goes into text; it does not earn its own EventKind.
            let note =
                (status != "success").then(|| match v.get("error").and_then(|x| x.as_str()) {
                    Some(e) => format!("{status}: {e}"),
                    None => status.to_string(),
                });
            events.push(Event {
                kind: EventKind::TurnEnd,
                text: note,
                timestamp: None,
                paths: vec![],
                tool: None,
                line: Some(lineno),
            });
            continue;
        }

        let role = v.get("role").and_then(|x| x.as_str()).unwrap_or("");
        let Some(blocks) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };

        if role == "user" {
            // user records hold text blocks only, without exception in the sample. Join them
            // first and unwrap the whole thing afterwards: `<timestamp>` and `<user_query>`
            // can land in different blocks.
            let raw = blocks
                .iter()
                .filter(|b| b.get("type").and_then(|x| x.as_str()) == Some("text"))
                .filter_map(|b| b.get("text").and_then(|x| x.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            if raw.trim().is_empty() {
                continue;
            }
            let (inner, ts) = unwrap_user_query(&raw);
            let kind = if is_synthetic_user_text(&inner) {
                // Injected by the runtime, not typed by a person. Recording it as Other
                // counts it as dropped without polluting UserPrompt — turn splitting keys
                // off UserPrompt, and a misjudgement cuts a false turn.
                EventKind::Other
            } else {
                EventKind::UserPrompt
            };
            events.push(Event::text(kind, inner, ts).at_line(lineno));
            continue;
        }

        for b in blocks {
            match b.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                "text" => {
                    if let Some(t) = b
                        .get("text")
                        .and_then(|x| x.as_str())
                        .filter(|t| !t.trim().is_empty())
                    {
                        // assistant records carry no time information at all, so this
                        // stays empty.
                        events
                            .push(Event::text(EventKind::AssistantReply, t, None).at_line(lineno));
                    }
                }
                "tool_use" => {
                    let name = b
                        .get("name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("tool")
                        .to_string();
                    let input = b.get("input");
                    collect_cwd_hints(input, &mut hints);
                    let paths = event_paths(&name, input);
                    events.push(Event {
                        kind: if is_write_tool(&name) {
                            EventKind::FileEdit
                        } else {
                            EventKind::ToolUse
                        },
                        text: Some(name.clone()),
                        timestamp: None,
                        paths,
                        tool: Some(name),
                        line: Some(lineno),
                    });
                }
                // There is no third kind. thinking lives in state.vscdb and never shows up
                // in the transcript, so not even `Other` is emitted — reporting "N thinking
                // blocks dropped" would be a lie.
                _ => {}
            }
        }
    }

    (
        Session {
            id: String::new(),
            runtime: "cursor".into(),
            cwd: None,
            events,
        },
        hints,
    )
}

/// `FileEdit` is decided by a **write-tool allowlist**, not by "it has a path, so it is an edit".
///
/// The heuristic the Claude Code adapter uses inflates the count badly on Cursor: `Read` carries
/// a `path` 2050 times against 1252 real writes (`StrReplace` 709 + `Write` 338 + `ApplyPatch`
/// 179 + `Delete` 26).
///
/// An allowlist works here because Cursor's tool names are a **stable built-in set**, not open
/// the way Claude Code's MCP tools are. The cost is that a new Cursor write tool has to be added
/// here — missing one only makes the edit count low, and never reports a read as a write.
fn is_write_tool(name: &str) -> bool {
    matches!(name, "Write" | "StrReplace" | "Delete" | "ApplyPatch")
}

/// Which **files** this call touched.
///
/// Deliberately excludes `working_directory` / `target_directory`: those say "where the command
/// ran", not which files were touched. They feed only the recovery in [`cwd_from`].
fn event_paths(tool: &str, input: Option<&serde_json::Value>) -> Vec<String> {
    let Some(v) = input else { return vec![] };

    // ApplyPatch's input is a **string**, not an object, and the paths sit in the patch header.
    if let Some(patch) = v.as_str() {
        if tool != "ApplyPatch" {
            return vec![];
        }
        return patch
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                for p in ["*** Add File: ", "*** Update File: ", "*** Delete File: "] {
                    if let Some(rest) = l.strip_prefix(p) {
                        return Some(rest.trim().to_string());
                    }
                }
                None
            })
            .filter(|p| !p.is_empty())
            .collect();
    }

    let mut out = vec![];
    for k in ["path", "file_path", "target_notebook", "notebook_path"] {
        if let Some(s) = v.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
            out.push(s.to_string());
        }
    }
    if let Some(arr) = v.get("paths").and_then(|x| x.as_array()) {
        out.extend(
            arr.iter()
                .filter_map(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from),
        );
    }
    out
}

/// Absolute-path candidates useful for recovering the cwd, wider than [`event_paths`] —
/// directories count too.
///
/// Capped: a large transcript holds thousands of tool calls, while finding the cwd usually takes
/// only the first few.
fn collect_cwd_hints(input: Option<&serde_json::Value>, out: &mut Vec<String>) {
    const CAP: usize = 64;
    if out.len() >= CAP {
        return;
    }
    let Some(v) = input else { return };
    let mut push = |s: &str| {
        if s.starts_with('/') && out.len() < CAP && !out.iter().any(|x| x == s) {
            out.push(s.to_string());
        }
    };
    for k in ["working_directory", "target_directory", "path", "file_path"] {
        if let Some(s) = v.get(k).and_then(|x| x.as_str()) {
            push(s);
        }
    }
    for k in ["target_directories", "paths"] {
        if let Some(arr) = v.get(k).and_then(|x| x.as_array()) {
            for s in arr.iter().filter_map(|x| x.as_str()) {
                push(s);
            }
        }
    }
}

/// Recovers the cwd from the transcript path.
///
/// # Why the slug cannot be inverted directly
///
/// The slug turns every non-alphanumeric character into `-`, so `/`, `-`, `.`, `_` and space all
/// become the same character and the information is already gone.
/// `Users-nana-Projects-AgentGit` can come from `/Users/nana/Projects/AgentGit` or from
/// `/Users/nana/Projects-AgentGit`.
///
/// # Verify against evidence instead
///
/// The transcript body holds plenty of **absolute paths** (carried by tool arguments), and the
/// cwd is an ancestor of one of them. So: take each candidate path's ancestors, compute the slug
/// of each, and compare against the slug in the path. **Equal means that is the one** — an exact
/// check rather than a guess, and it holds even once the path no longer exists.
///
/// Take the longest matching ancestor (`ancestors()` already runs longest to shortest), because
/// `/a/b` and `/a` have different slugs and only one of them can match.
///
/// Three kinds of slug are not paths to begin with (`empty-window`, a bare numeric workspace id,
/// `var-folders-...`); nothing matches then, and this honestly returns None.
fn cwd_from(path: &Path, hints: &[String]) -> Option<String> {
    let slug = slug_from_path(path)?;
    for h in hints {
        for anc in Path::new(h).ancestors() {
            // Stop at the root: `slug_for("/")` is the empty string and equals no real slug,
            // but stopping explicitly is clearer.
            if anc.parent().is_none() {
                break;
            }
            if slug_for(anc) == slug {
                return Some(anc.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// Pulls the slug out of `.../projects/<slug>/agent-transcripts/...`.
fn slug_from_path(path: &Path) -> Option<String> {
    let comps: Vec<&str> = path
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let i = comps.iter().position(|c| *c == TRANSCRIPTS)?;
    (i > 0).then(|| comps[i - 1].to_string())
}

/// Layer one: strip the wrapper Cursor puts around human input.
///
/// Returns (inner body, timestamp).
///
/// `<timestamp>` is **Cursor's only source of time**, so do not lose it — assistant records
/// carry no time information at all. It is a localized human-readable string
/// (`Friday, Jul 31, 2026, 11:12 PM (UTC+8)`), not ISO8601.
fn unwrap_user_query(raw: &str) -> (String, Option<String>) {
    let ts = between(raw, "<timestamp>", "</timestamp>").and_then(|s| to_rfc3339(s.trim()));
    // Use the **last** closing tag: the wrapper is the outermost layer, and a user can
    // perfectly well paste in text that itself contains `</user_query>`.
    let inner = match (raw.find("<user_query>"), raw.rfind("</user_query>")) {
        (Some(a), Some(b)) if b > a => raw[a + "<user_query>".len()..b].trim().to_string(),
        // Exactly one record in the sample has no wrapper (the interrupted-continuation
        // injection). The whole body is the candidate then.
        _ => raw.trim().to_string(),
    };
    (inner, ts)
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let a = s.find(open)? + open.len();
    let b = s[a..].find(close)? + a;
    Some(&s[a..b])
}

/// `Friday, Jul 31, 2026, 11:12 PM (UTC+8)` → RFC3339.
///
/// # Why it is converted
///
/// `Event::timestamp` is used everywhere else as machine time (sorting, display). Put a
/// localized human-readable string in that slot and downstream either displays something
/// inexplicable or fails to parse.
///
/// # Why an unparseable string yields None rather than being propagated unchanged
///
/// The format of this string depends on Cursor's locale (every sample seen is English, but
/// nothing guarantees that). Another language stops parsing, and then **no timestamp** beats **a
/// timestamp downstream cannot read**: absence is the case every consumer already handles
/// (assistant records carry no time at all), while a malformed value makes each of them fail in
/// its own way.
fn to_rfc3339(s: &str) -> Option<String> {
    use chrono::{FixedOffset, NaiveDateTime, TimeZone};

    let (dt, off) = s.split_once(" (UTC")?;
    let off = off.trim_end_matches(')');
    let naive = NaiveDateTime::parse_from_str(dt.trim(), "%A, %b %d, %Y, %I:%M %p").ok()?;

    // The offset is written `+8` / `+08:00` / `-5:30`, not the standard `%:z`.
    let (sign, rest) = match off.as_bytes().first()? {
        b'+' => (1, &off[1..]),
        b'-' => (-1, &off[1..]),
        _ => (1, off),
    };
    let (h, m) = match rest.split_once(':') {
        Some((h, m)) => (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?),
        None => (rest.parse::<i32>().ok()?, 0),
    };
    let tz = FixedOffset::east_opt(sign * (h * 3600 + m * 60))?;
    Some(tz.from_local_datetime(&naive).single()?.to_rfc3339())
}

/// Layer two: whether the inner body was injected by the runtime.
///
/// # The order cannot be reversed
///
/// **Injections and human input share the same `<user_query>` wrapper**, so the shell comes off
/// before the inner text is judged. The strategy the other two adapters use — match a known
/// prefix against the whole body — fails outright on Cursor, where every record starts with
/// `<timestamp>` or `<user_query>`.
///
/// # The four kinds observed (`samples/cursor/synthetic-user-text.txt`)
///
/// | Count | What |
/// |---|---|
/// | 12 | wakes the parent agent once a subagent finishes |
/// | 9 | wakes once a background task finishes |
/// | 2 | self-fork opening line |
/// | 1 | interrupted continuation (**no `<user_query>` wrapper**) |
///
/// # "It appears more than once" is not a signal of synthesis
///
/// The natural fallback idea is "the same sentence more than once means an injection". **The
/// observations refute it**: human input also repeats heavily (the same sentence appears 4
/// times), because Cursor's fork / multitasking **copies** the parent session's history into the
/// child session's transcript. That rule would wipe out human input in swathes.
///
/// `Start multitasking` (5 occurrences) is undecidable: it comes from the user clicking a UI
/// button, reads like a command, and counts as user intent by origin. It is not added — missing
/// it only makes the count high, while misjudging it wipes out human input.
fn is_synthetic_user_text(text: &str) -> bool {
    const INJECTED: &[&str] = &[
        "Perform any necessary follow-up actions in response to the subagent completion",
        "Briefly inform the user about the task result and perform any follow-up action",
        "You are the forked subagent; continue executing your task.",
        "Your previous response was interrupted. Continue from where you left off.",
    ];
    let s = text.trim_start();
    INJECTED.iter().any(|p| s.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Differs from the Claude Code slug by one leading character. Getting it backwards makes
    /// the whole reverse lookup fail silently.
    #[test]
    fn slug_drops_the_leading_dash() {
        let p = Path::new("/Users/nana/Projects/AgentGit");
        assert_eq!(slug_for(p), "Users-nana-Projects-AgentGit");
        assert_eq!(
            crate::domain::store::slug_for(p),
            "-Users-nana-Projects-AgentGit",
            "the Claude Code slug keeps the leading `-`"
        );
    }

    #[test]
    fn parses_the_two_record_shapes() {
        let text = concat!(
            r#"{"role":"user","message":{"content":[{"type":"text","text":"<timestamp>Friday, Jul 31, 2026, 11:12 PM (UTC+8)</timestamp>\n<user_query>\nfix this bug for me\n</user_query>"}]}}"#,
            "\n",
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"sure"},{"type":"tool_use","name":"Read","input":{"path":"/repo/a.rs"}},{"type":"tool_use","name":"StrReplace","input":{"path":"/repo/a.rs"}}]}}"#,
            "\n",
            r#"{"type":"turn_ended","status":"success"}"#,
            "\n",
        );
        let s = Cursor.parse(text).unwrap();
        let c = s.counts();
        assert_eq!(c.prompts, 1);
        assert_eq!(c.replies, 1);
        assert_eq!(c.tools, 1, "Read is a ToolUse");
        assert_eq!(c.edits, 1, "StrReplace is a FileEdit");
        assert_eq!(
            s.gist(20).as_deref(),
            Some("fix this bug for me"),
            "the wrapper must be stripped"
        );
        assert_eq!(
            s.events[0].timestamp.as_deref(),
            Some("2026-07-31T23:12:00+08:00")
        );
    }

    /// `FileEdit` is decided by the write-tool allowlist, not by "it has a path, so it is an
    /// edit".
    ///
    /// `Read` carries a `path` 2050 times against 1252 real writes, so a heuristic inflates the
    /// count badly.
    #[test]
    fn reads_are_not_edits() {
        for t in ["Write", "StrReplace", "Delete", "ApplyPatch"] {
            assert!(is_write_tool(t), "{t} must count as a write");
        }
        for t in ["Read", "Glob", "Grep", "Shell", "ReadLints", "TodoWrite"] {
            assert!(!is_write_tool(t), "{t} must not count as a write");
        }

        // A read's path is still recorded — "which files this session touched" is worth
        // having.
        let text = r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"path":"/repo/a.rs"}}]}}"#;
        let s = Cursor.parse(text).unwrap();
        assert_eq!(s.events[0].kind, EventKind::ToolUse);
        assert_eq!(s.events[0].paths, vec!["/repo/a.rs"]);
    }

    /// ApplyPatch's `input` is a **string**, not an object, and the paths sit in the patch
    /// header.
    #[test]
    fn apply_patch_input_is_a_string() {
        let line = serde_json::json!({
            "role": "assistant",
            "message": {"content": [{
                "type": "tool_use",
                "name": "ApplyPatch",
                "input": "*** Begin Patch\n*** Add File: /repo/new.md\n+hi\n*** Update File: /repo/old.rs\n*** End Patch"
            }]}
        })
        .to_string();
        let s = Cursor.parse(&line).unwrap();
        assert_eq!(s.events[0].kind, EventKind::FileEdit);
        assert_eq!(s.events[0].paths, vec!["/repo/new.md", "/repo/old.rs"]);
    }

    /// Injection is judged **after the shell comes off**.
    ///
    /// Injections and human input share the same `<user_query>` wrapper, so matching a prefix
    /// against the whole body sees `<timestamp>` at the start of every record and recognizes
    /// none of them.
    #[test]
    fn injection_is_detected_on_the_inner_text() {
        let wrapped = r#"{"role":"user","message":{"content":[{"type":"text","text":"<timestamp>Friday, Jul 31, 2026, 11:12 PM (UTC+8)</timestamp>\n<user_query>\nPerform any necessary follow-up actions in response to the subagent completion above.\n</user_query>"}]}}"#.to_string();
        let real = r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nfix it\n</user_query>"}]}}"#;
        let s = Cursor.parse(&format!("{wrapped}\n{real}\n")).unwrap();
        let c = s.counts();
        assert_eq!(c.prompts, 1, "only one record is typed by a person");
        assert_eq!(c.dropped, 1, "an injection is recorded as Other");
        assert_eq!(s.gist(10).as_deref(), Some("fix it"));
    }

    /// The interrupted-continuation injection carries **no wrapper** and is still recognized.
    #[test]
    fn unwrapped_injection_still_detected() {
        let line = r#"{"role":"user","message":{"content":[{"type":"text","text":"Your previous response was interrupted. Continue from where you left off."}]}}"#;
        assert_eq!(Cursor.parse(line).unwrap().counts().prompts, 0);
    }

    /// Human input must never be misjudged — that leaves `agit log` unable to say what this
    /// stretch of work was.
    #[test]
    fn real_input_is_never_synthetic() {
        for s in [
            "fix the bug where photo thumbnails come out rotated",
            "continue",
            // Captured-evidence fixture (AGENTS.md exception iv): this is one of the repeated
            // human inputs counted in `samples/cursor/synthetic-user-text.txt`.
            "改一下吧",
            // A Python traceback the user pasted. Exactly why "starts with `<`, so it is an
            // injection" cannot be the rule.
            "<module>\n  File \"x.py\", line 3",
            // Produced by clicking a UI button: it reads like a command, but its origin is
            // user intent. Missing it only makes the count high.
            "Start multitasking",
            "",
        ] {
            assert!(
                !is_synthetic_user_text(s),
                "must not be judged an injection: {s}"
            );
        }
    }

    /// "the same sentence appears more than once" is not a test for injection.
    ///
    /// Cursor's fork / multitasking **copies** the parent session's history into the child
    /// session's transcript, so human input repeats heavily — the same sentence appears 4
    /// times.
    #[test]
    fn repetition_is_not_a_signal() {
        // Captured-evidence fixture (AGENTS.md exception iv): the repeated line is taken from
        // `samples/cursor/synthetic-user-text.txt`.
        let one = r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\n改一下吧\n</user_query>"}]}}"#;
        let s = Cursor
            .parse(&format!("{one}\n{one}\n{one}\n{one}\n"))
            .unwrap();
        assert_eq!(
            s.counts().prompts,
            4,
            "four repeated human inputs; none may be lost"
        );
    }

    /// A failed `turn_ended` is still a turn end, with the interruption reason in text.
    #[test]
    fn failed_turns_still_end_the_turn() {
        let text = concat!(
            r#"{"type":"turn_ended","status":"success"}"#,
            "\n",
            r#"{"type":"turn_ended","status":"error","error":"User aborted request"}"#,
            "\n",
        );
        let s = Cursor.parse(text).unwrap();
        assert_eq!(s.events.len(), 2);
        assert!(s.events.iter().all(|e| e.kind == EventKind::TurnEnd));
        assert!(
            s.events[0].text.is_none(),
            "a successful turn needs no note"
        );
        assert_eq!(
            s.events[1].text.as_deref(),
            Some("error: User aborted request")
        );
        // A turn-end marker is not content and enters no activity count.
        let c = s.counts();
        assert_eq!((c.prompts, c.replies, c.tools, c.dropped), (0, 0, 0, 0));
    }

    /// `turn_ended` exists only from 2026-07-30, and transcripts before it carry none. Its
    /// absence must not error.
    #[test]
    fn transcripts_without_turn_ended_are_fine() {
        let text = r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nhi\n</user_query>"}]}}"#;
        assert_eq!(Cursor.parse(text).unwrap().counts().prompts, 1);
    }

    /// `compactions` on the Cursor side is always 0, and **that is not a bug**.
    ///
    /// Compaction output is encrypted into `state.vscdb` and leaves no trace in the transcript
    /// (the four sessions with the highest context usage have transcripts shaped exactly like
    /// ordinary ones). This pins that expectation, so nobody who later sees the 0 goes and
    /// "fixes" it.
    #[test]
    fn compaction_is_invisible_in_cursor_transcripts() {
        let text = r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nhi\n</user_query>"}]}}"#;
        assert_eq!(Cursor.parse(text).unwrap().counts().compactions, 0);
    }

    /// thinking emits not even `Other` — it never appears in the transcript, and reporting
    /// "N dropped" would be a lie.
    #[test]
    fn thinking_is_not_reported_as_dropped() {
        let text =
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"answer"}]}}"#;
        assert_eq!(Cursor.parse(text).unwrap().counts().dropped, 0);
    }

    #[test]
    fn corrupt_lines_are_skipped_not_fatal() {
        let good = r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nhi\n</user_query>"}]}}"#;
        let s = Cursor
            .parse(&format!("{good}\nNOT JSON\n{{trunca"))
            .unwrap();
        assert_eq!(s.counts().prompts, 1);
    }

    /// Every event remembers its source line — the web transcript uses it to go back for the
    /// tool arguments.
    #[test]
    fn events_point_back_at_their_source_line() {
        let text = concat!(
            r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nask\n</user_query>"}]}}"#,
            "\n",
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"answer"},{"type":"tool_use","name":"Shell","input":{"command":"ls"}}]}}"#,
            "\n",
        );
        let s = Cursor.parse(text).unwrap();
        assert_eq!(
            s.events.iter().map(|e| e.line).collect::<Vec<_>>(),
            vec![Some(0), Some(1), Some(1)]
        );
    }

    /// A timestamp becomes a form a machine can read.
    #[test]
    fn human_timestamp_becomes_rfc3339() {
        assert_eq!(
            to_rfc3339("Friday, Jul 31, 2026, 11:12 PM (UTC+8)").as_deref(),
            Some("2026-07-31T23:12:00+08:00")
        );
        // Single-digit day, AM, and an offset carrying minutes.
        assert_eq!(
            to_rfc3339("Tuesday, Jun 2, 2026, 9:05 AM (UTC+05:30)").as_deref(),
            Some("2026-06-02T09:05:00+05:30")
        );
        // Unrecognized input yields None — a malformed value is worse than a missing one; see
        // the function docs. The Chinese-locale form is the fixture (AGENTS.md exception iii):
        // another locale must not parse.
        assert_eq!(to_rfc3339("2026年7月31日 23:12"), None);
        assert_eq!(to_rfc3339(""), None);
    }

    /// The cwd is verified exactly — an ancestor's slug equals the slug in the path — not
    /// guessed.
    #[test]
    fn cwd_is_recovered_by_verifying_ancestors() {
        let path = Path::new(
            "/home/u/.cursor/projects/Users-nana-Projects-AgentGit/agent-transcripts/abc/abc.jsonl",
        );
        let text = r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"path":"/Users/nana/Projects/AgentGit/agent-git/src/lib.rs"}}]}}"#;
        let hints = parse_text(text).1;
        assert_eq!(
            cwd_from(path, &hints).as_deref(),
            Some("/Users/nana/Projects/AgentGit")
        );
    }

    /// Three kinds of slug are not paths at all; when nothing can be recovered, say so.
    #[test]
    fn unreversible_slugs_yield_no_cwd() {
        let text = r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"path":"/Users/nana/Projects/AgentGit/x.rs"}}]}}"#;
        let hints = parse_text(text).1;
        for slug in ["empty-window", "1785313507111"] {
            let p = PathBuf::from(format!(
                "/home/u/.cursor/projects/{slug}/agent-transcripts/abc/abc.jsonl"
            ));
            assert_eq!(
                cwd_from(&p, &hints),
                None,
                "{slug} must not be forced into a cwd"
            );
        }
    }

    /// The transcript has no id or cwd; only `parse_at` can produce them.
    #[test]
    fn bare_parse_cannot_know_id_or_cwd() {
        let s = Cursor.parse("").unwrap();
        assert!(s.id.is_empty());
        assert!(s.cwd.is_none());
        assert_eq!(s.runtime, "cursor");
    }

    #[test]
    fn parse_at_takes_id_from_the_path() {
        let d = tempfile::tempdir().unwrap();
        let dir = d
            .path()
            .join("projects/Users-nana-Projects-AgentGit/agent-transcripts/ID-1");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("ID-1.jsonl");
        std::fs::write(
            &f,
            concat!(
                r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nhi\n</user_query>"}]}}"#,
                "\n",
                r#"{"role":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"path":"/Users/nana/Projects/AgentGit/a.rs"}}]}}"#,
                "\n",
            ),
        )
        .unwrap();

        let s = Cursor.parse_at(&f).unwrap();
        assert_eq!(s.id, "ID-1");
        assert_eq!(s.cwd.as_deref(), Some("/Users/nana/Projects/AgentGit"));
        assert_eq!(s.counts().prompts, 1);
    }

    /// "It cannot be installed" is said **before any work starts**, and said so it can be acted
    /// on.
    ///
    /// The worst failure shape is "the command returns success and the user opens Cursor to
    /// nothing".
    #[test]
    fn install_refuses_loudly() {
        assert!(!Cursor.installable());
        let e = Cursor
            .install("{}", "new-id", Path::new("/repo"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("state.vscdb"), "the reason must be stated: {e}");
        assert!(
            e.contains("--as claude-code"),
            "the next step must be given: {e}"
        );
    }

    /// Cursor as a **source** is complete: IR → Claude Code works.
    #[test]
    fn cursor_is_a_clean_source_for_other_runtimes() {
        let text = concat!(
            r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nrefactor the payments module\n</user_query>"}]}}"#,
            "\n",
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"sure"}]}}"#,
            "\n",
        );
        let ir = Cursor.parse(text).unwrap();
        let out = super::super::claude_code::ClaudeCode
            .render(&ir, "nid", Path::new("/repo"))
            .unwrap();
        assert_eq!(out.lines().count(), 2);
        assert!(out.contains("refactor the payments module"));
        assert!(
            !out.contains("user_query"),
            "the wrapper must not leak into the target runtime"
        );
    }
}
