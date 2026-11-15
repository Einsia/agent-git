//! Pi (pi-coding-agent) transcripts.
//!
//! Pi writes one append-only JSONL per session under its agent directory
//! (`PI_CODING_AGENT_DIR`, or `~/.pi/agent` otherwise), in
//! `sessions/--<cwd-slug>--/<timestamp>_<sessionId>.jsonl`. The header is a `session` record of
//! version 3, and every later record is a tree node with its own `id` and `parentId`; the leaf is
//! the last record in file order and is never written down. Compaction, branches and labels are
//! records of their own types, so the conversation itself is only the `message` records on the
//! path from the root to the leaf.
//!
//! Pi rewrites a file only to migrate one it wrote before version 3 or to repair an empty one, so
//! a file that is only appended to is read as it stands. A file Pi rewrote is read whole again,
//! which the capture already does for any file whose bytes moved.

use super::native_message::{self as message, Call, Output};
use super::{
    Adapter, Capability, Event, EventKind, Installed, Next, OpenCall, Session, SessionRef,
    ToolDetails,
};
use crate::Result;
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The session format this build reads. Pi migrates anything older on load, so a file at another
/// version was not written by a Pi this build knows.
const VERSION: u64 = 3;

/// What Pi names a tool when a call's name did not survive the conversion.
const UNKNOWN_TOOL: &str = "unknown";

/// The runtime's own directory: `PI_CODING_AGENT_DIR` when it is set and not empty, otherwise
/// `~/.pi/agent`.
pub fn home() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("PI_CODING_AGENT_DIR").filter(|root| !root.is_empty()) {
        return Ok(PathBuf::from(root));
    }
    Ok(crate::infra::config::user_home()
        .context("the user home is not set")?
        .join(".pi/agent"))
}

/// Where a Pi this build knows keeps its session files.
pub fn sessions_dir() -> Result<PathBuf> {
    Ok(home()?.join("sessions"))
}

/// The folder `PI_CODING_AGENT_SESSION_DIR` moves Pi's sessions to, when it names one. Pi keeps
/// them there flat, one file per session. The `sessionDir` setting can move them too; agit does
/// not read Pi's settings, so a session kept only there is not found by listing.
fn moved_sessions_dir() -> Option<PathBuf> {
    std::env::var_os("PI_CODING_AGENT_SESSION_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
}

/// Whether `path` is a Pi session file agit may read: a regular `.jsonl` file named
/// `<timestamp>_<id>.jsonl`, in a project folder (`--<cwd>--`) of Pi's sessions folder or directly
/// in the folder `PI_CODING_AGENT_SESSION_DIR` names, judged by where it really is, and opening
/// with a Pi session header. Any other path a caller names is not read.
pub fn is_session_file(path: &Path) -> bool {
    is_session_file_in(
        sessions_dir().ok().as_deref(),
        moved_sessions_dir().as_deref(),
        path,
    )
}

/// [`is_session_file`] for Pi's sessions folder `sessions` and the folder `moved` that
/// `PI_CODING_AGENT_SESSION_DIR` names.
fn is_session_file_in(sessions: Option<&Path>, moved: Option<&Path>, path: &Path) -> bool {
    let Ok(real) = path.canonicalize() else {
        return false;
    };
    let named = real
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".jsonl"))
        .and_then(|stem| stem.split_once('_'))
        .is_some_and(|(stamp, id)| !stamp.is_empty() && valid_id(id));
    if !named || !real.is_file() {
        return false;
    }
    let under = |root: Option<&Path>, depth: usize| {
        root.and_then(|root| root.canonicalize().ok())
            .and_then(|root| real.strip_prefix(&root).ok().map(Path::to_path_buf))
            .is_some_and(|relative| {
                let parts: Vec<_> = relative.components().collect();
                parts.len() == depth
                    && (depth == 1
                        || parts[0].as_os_str().to_str().is_some_and(|folder| {
                            folder.len() > 4 && folder.starts_with("--") && folder.ends_with("--")
                        }))
            })
    };
    (under(sessions, 2) || under(moved, 1)) && header_of(&real).is_some()
}

/// The session header `path` opens with, read from its first line alone.
fn header_of(path: &Path) -> Option<Value> {
    use std::io::{BufRead as _, Read as _};
    let file = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(file)
        .take(64 * 1024)
        .read_line(&mut line)
        .ok()?;
    serde_json::from_str::<Value>(&line)
        .ok()
        .filter(|value| value["type"] == "session" && value["version"] == VERSION)
}

/// The session directory Pi uses for `cwd`: `sessions/--<slug>--`, where every `/`, `\` and `:`
/// of the path, and its leading slash, becomes `-`.
pub fn project_dir(cwd: &Path) -> Result<PathBuf> {
    Ok(sessions_dir()?.join(format!("--{}--", slug(cwd))))
}

fn slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .trim_start_matches(['/', '\\'])
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' => '-',
            other => other,
        })
        .collect()
}

/// Whether `id` is a session id Pi accepts: non-empty, only letters, digits, `-`, `_` and `.`,
/// and starting and ending with a letter or digit. Pi refuses anything else, so a session installed
/// under another id would never open.
pub fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    match (chars.next(), chars.next_back()) {
        (Some(first), Some(last)) => {
            alphanumeric(first) && alphanumeric(last) && chars.all(id_char)
        }
        (Some(only), None) => alphanumeric(only),
        (None, _) => false,
    }
}

fn alphanumeric(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
}

fn id_char(ch: char) -> bool {
    alphanumeric(ch) || matches!(ch, '-' | '_' | '.')
}

/// A session id Pi accepts, with no `-` in it. Pi's own ids are uuidv7, which hold `-`, and those
/// stay as Pi wrote them; an id minted here is only ever a fresh identity, so it can be the shape
/// that also makes a filename unambiguous (see [`session_id`]).
fn fresh_id() -> String {
    uuid::Uuid::now_v7().simple().to_string()
}

pub struct Pi;

impl Adapter for Pi {
    fn id(&self) -> &'static str {
        "pi"
    }

    fn cli(&self) -> &'static str {
        "pi"
    }

    fn capability(&self) -> Capability {
        Capability::Resumable
    }

    fn format(&self) -> &'static str {
        "pi"
    }

    fn requires_turn_end(&self) -> bool {
        true
    }

    fn start_command(&self, cwd: &Path) -> Option<String> {
        Some(launch(None, cwd, None, None))
    }

    fn resume_command(
        &self,
        id: &str,
        cwd: &Path,
        prompt: Option<&str>,
        system: Option<&str>,
    ) -> Option<String> {
        Some(launch(Some(id), cwd, prompt, system))
    }

    fn native_files_root(&self) -> Result<PathBuf> {
        sessions_dir()
    }

    /// The sessions whose header names `repo`. Pi keeps a project's sessions in a folder named
    /// after it, or flat in a moved folder, and decides by the header either way.
    fn sessions_for(&self, repo: &Path) -> Result<Vec<SessionRef>> {
        let same = |cwd: &str| {
            Path::new(cwd) == repo
                || Path::new(cwd)
                    .canonicalize()
                    .is_ok_and(|cwd| repo.canonicalize().is_ok_and(|repo| cwd == repo))
        };
        Ok(self
            .all_sessions()?
            .into_iter()
            .filter(|session| session.cwd.as_deref().is_some_and(same))
            .collect())
    }

    fn all_sessions(&self) -> Result<Vec<SessionRef>> {
        list_in(&sessions_dir()?, moved_sessions_dir().as_deref())
    }

    fn resolve(&self, id: &str, cwd: Option<&Path>) -> Option<PathBuf> {
        if !valid_id(id) {
            return None;
        }
        if let Some(cwd) = cwd
            && let Ok(dir) = project_dir(cwd)
            && let Some(path) = file_in(&dir, id)
        {
            return Some(path);
        }
        file_in(&sessions_dir().ok()?, id).or_else(|| file_in(&moved_sessions_dir()?, id))
    }

    fn parse(&self, raw: &str) -> Result<Session> {
        Ok(parse(raw)?.session)
    }

    /// A file Pi wrote opens with its session header, so a file without one is not a Pi session
    /// and is not read as one. Only a published copy, which never comes from Pi's folders, has
    /// no header.
    fn parse_at(&self, path: &Path) -> Result<Session> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let session = self.parse(&text)?;
        ensure!(
            !session.id.is_empty(),
            "Pi transcript has no session header"
        );
        Ok(session)
    }

    fn record_identity(&self, value: &Value) -> Option<(String, Option<String>)> {
        (value["type"] == "session").then(|| {
            Some((
                value["id"].as_str()?.into(),
                value["cwd"].as_str().map(str::to_owned),
            ))
        })?
    }

    fn record_class(&self, record: &Value) -> super::RecordClass {
        use super::RecordClass::{Content, Metadata, Unknown};
        match record["type"].as_str() {
            // Bookkeeping that carries no text, path, file body or copy of a prompt.
            Some(
                "thinking_level_change"
                | "model_change"
                | "usage"
                | "label"
                | "session_info"
                | "leaf"
                | "context_edit",
            ) => Metadata,
            // The conversation and everything that can hold the person's words, a tool's input or
            // output, a file body, or a summary of them.
            Some(
                "session" | "message" | "compaction" | "branch_summary" | "custom"
                | "custom_message",
            ) => Content,
            _ => Unknown,
        }
    }

    fn line_details(&self, value: &Value) -> super::detail::LineDetails {
        use super::detail::{EventDetail, LineDetails, plaintext, summary};
        let mut details = LineDetails::default();
        if matches!(
            value["type"].as_str(),
            Some("compaction" | "branch_summary")
        ) {
            details.compact = Some(summary(value["summary"].as_str().unwrap_or_default()));
        }
        let body = if value["type"] == "message" {
            &value["message"]
        } else if value["type"] == "custom_message" {
            value
        } else {
            return details;
        };
        if matches!(body["role"].as_str(), Some("user" | "assistant"))
            && let Some(blocks) = body["content"].as_array()
        {
            for block in blocks {
                match block["type"].as_str() {
                    Some("toolCall") => details.tools.push(EventDetail {
                        input: Some(message::input(&block["arguments"])),
                        ..Default::default()
                    }),
                    Some("text") => {}
                    Some("thinking") => details
                        .opaque
                        .push(plaintext(block["thinking"].as_str().unwrap_or_default())),
                    _ => details.opaque.push(EventDetail::default()),
                }
            }
        }
        details
    }

    fn event_details(
        &self,
        raw: &str,
        session: &Session,
    ) -> Vec<Option<super::detail::EventDetail>> {
        let rows = rows_of(raw).ok();
        let selected = rows.as_ref().and_then(|rows| active_lines(rows).ok());
        super::detail::project(self, raw, session, |line| {
            selected.as_ref().is_some_and(|lines| lines.contains(&line))
        })
    }

    fn tool_details(&self, raw: &str, session: &Session, output: bool) -> ToolDetails {
        parse(raw)
            .map(|parsed| message::details(session, &parsed.calls, &parsed.outputs, output))
            .unwrap_or_default()
    }

    fn open_tool_calls(&self, raw: &str) -> Vec<OpenCall> {
        parse(raw)
            .map(|parsed| message::open_calls(&parsed.calls, &parsed.outputs, "toolCall"))
            .unwrap_or_default()
    }

    fn mint_id(&self) -> String {
        fresh_id()
    }

    fn render(&self, session: &Session, id: &str, cwd: &Path) -> Result<String> {
        self.render_with(session, id, cwd, &ToolDetails::default())
    }

    fn render_with(
        &self,
        session: &Session,
        id: &str,
        cwd: &Path,
        details: &ToolDetails,
    ) -> Result<String> {
        ensure!(valid_id(id), "Pi refuses session id `{id}`");
        let mut out = format!(
            "{}\n",
            json!({"type":"session","version":VERSION,"id":id,"cwd":cwd.to_string_lossy(),"timestamp":stamp(None)})
        );
        let mut parent = Value::Null;
        for (index, event) in session.events.iter().enumerate() {
            let key = message::remint(id, &index.to_string());
            let ts = event
                .timestamp
                .as_deref()
                .unwrap_or("2026-01-01T00:00:00.000Z");
            let mut value = json!({"type":"message","id":key,"parentId":parent,"timestamp":ts,"message":{"timestamp":message::millis(Some(ts))}});
            match event.kind {
                EventKind::UserPrompt
                | EventKind::UserInterjection
                | EventKind::AssistantReply
                | EventKind::CompactSummary
                | EventKind::CompactFiltered => {
                    value["message"]["role"] = json!(if event.kind == EventKind::AssistantReply {
                        "assistant"
                    } else {
                        "user"
                    });
                    value["message"]["content"] =
                        json!([{"type":"text","text":event.text.as_deref().unwrap_or("")}]);
                    if event.kind == EventKind::AssistantReply {
                        value["message"]["stopReason"] = json!("stop");
                        value["message"]["usage"] = json!({"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}});
                    }
                }
                EventKind::ToolUse | EventKind::FileEdit if !details.is_receipt(index) => {
                    let detail = details.get(index).cloned().unwrap_or_default();
                    value["message"]["role"] = json!("assistant");
                    value["message"]["stopReason"] = json!("toolUse");
                    value["message"]["content"] = json!([{
                        "type":"toolCall",
                        "id":key,
                        "name":event.tool.as_deref().unwrap_or(UNKNOWN_TOOL),
                        "arguments":detail.input.unwrap_or(json!({}))
                    }]);
                    out.push_str(&format!("{value}\n"));
                    value["id"] = json!(message::remint(id, &format!("result:{index}")));
                    value["parentId"] = json!(key);
                    value["message"] = json!({
                        "role":"toolResult",
                        "toolCallId":key,
                        "toolName":event.tool.as_deref().unwrap_or(UNKNOWN_TOOL),
                        "content":[{"type":"text","text":detail.output.as_deref().unwrap_or(crate::domain::install::OPEN_CALL_PLACEHOLDER_OUTPUT)}],
                        "isError":detail.error,
                        "timestamp":message::millis(Some(ts))
                    });
                }
                _ => continue,
            }
            parent = value["id"].clone();
            out.push_str(&format!("{value}\n"));
        }
        Ok(out)
    }

    fn localize(&self, raw: &str, id: &str, cwd: &Path) -> Result<String> {
        ensure!(valid_id(id), "Pi refuses session id `{id}`");
        let raw = crate::domain::install::repair_protocol_ids(raw)?;
        let parsed = parse(&raw)?;
        let mut rows = records(&raw)?;
        if rows.first().is_none_or(|value| value["type"] != "session") {
            rows.insert(
                0,
                json!({"type":"session","version":VERSION,"id":id,"cwd":cwd.to_string_lossy(),"timestamp":stamp(None)}),
            );
        }
        for value in &mut rows {
            if value["type"] == "session" {
                value["id"] = json!(id);
                value["cwd"] = json!(cwd.to_string_lossy());
                value
                    .as_object_mut()
                    .context("session header")?
                    .remove("parentSession");
            }
        }
        let indexed: Vec<_> = rows.iter().cloned().enumerate().collect();
        let active = active_lines(&indexed)?;
        let mut parent = active
            .last()
            .and_then(|line| rows.get(*line))
            .map(|row| row["id"].clone())
            .unwrap_or(Value::Null);
        for call in message::open_calls(&parsed.calls, &parsed.outputs, "toolCall") {
            let key = message::remint(id, &format!("missing:{}", call.call_id));
            rows.push(json!({
                "type":"message",
                "id":key,
                "parentId":parent,
                "timestamp":stamp(None),
                "message":{
                    "role":"toolResult",
                    "toolCallId":call.call_id,
                    "toolName":call.name,
                    "content":[{"type":"text","text":crate::domain::install::OPEN_CALL_PLACEHOLDER_OUTPUT}],
                    "isError":true,
                    "timestamp":0
                }
            }));
            parent = json!(key);
        }
        Ok(rows.into_iter().map(|row| format!("{row}\n")).collect())
    }

    fn install(&self, raw: &str, id: &str, cwd: &Path) -> Result<Installed> {
        install_at(&sessions_dir()?, raw, id, cwd)
    }
}

/// Installs `raw` as session `id` of `cwd` in Pi's sessions folder `sessions`: a file of its own in
/// the folder Pi keeps `cwd`'s sessions in, opened by its path.
fn install_at(sessions: &Path, raw: &str, id: &str, cwd: &Path) -> Result<Installed> {
    let content = Pi.localize(raw, id, cwd)?;
    let dir = sessions.join(format!("--{}--", slug(cwd)));
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let path = dir.join(format!("{}_{id}.jsonl", file_stamp()));
    ensure!(
        !path.exists(),
        "Pi already has a session file at {}",
        path.display()
    );
    std::fs::write(&path, content.as_bytes())
        .with_context(|| format!("cannot write {}", path.display()))?;
    // By its path: an id would make Pi search every project's sessions for it.
    let session = path.to_string_lossy().into_owned();
    Ok(Installed {
        next: Next::Resume(launch(Some(&session), cwd, None, None)),
        path,
    })
}

/// The Pi sessions in the project folders of `sessions`, and directly in `moved`.
fn list_in(sessions: &Path, moved: Option<&Path>) -> Result<Vec<SessionRef>> {
    let mut found = vec![];
    for (root, depth) in [(Some(sessions), 2), (moved, 1)] {
        let Some(root) = root.filter(|root| root.is_dir()) else {
            continue;
        };
        let entries = walkdir::WalkDir::new(root)
            .min_depth(depth)
            .max_depth(depth);
        for entry in entries.into_iter().filter_map(|entry| entry.ok()) {
            if is_session_file_in(Some(sessions), moved, entry.path())
                && let Some(session) = session_of(entry.path())?
            {
                found.push(session);
            }
        }
    }
    Ok(found)
}

/// The shell command that starts Pi in `cwd`, resuming `id` when there is one. A prompt is a
/// positional argument after `--`, which is what stops Pi reading a leading `-` as its own flag;
/// an extra system prompt is a file, since Pi reads `--append-system-prompt` as a path when the
/// text holds a newline and the text here is the runtime's own.
fn launch(id: Option<&str>, cwd: &Path, prompt: Option<&str>, system: Option<&str>) -> String {
    let mut command = format!("cd {} && pi", super::shell_arg(&cwd.to_string_lossy()));
    if let Some(session) = id {
        command.push_str(&format!(" --session {}", super::shell_id(session)));
    }
    if let Some(system) = system {
        command.push_str(&format!(
            " --append-system-prompt {}",
            super::shell_arg(system)
        ));
    }
    if let Some(prompt) = prompt {
        command.push_str(&format!(" -- {}", super::shell_arg(prompt)));
    }
    command
}

/// `path` when it is a session file whose name ends in `_<id>` and whose header names `id`. Pi
/// allows `.` in an id and a filename may carry a timestamp, so the id is not the file's stem and
/// the header decides.
fn file_in(dir: &Path, id: &str) -> Option<PathBuf> {
    let suffix = format!("_{id}.jsonl");
    let mut found = None;
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(&suffix))
            && header_names(path, id)
        {
            if found.is_some() {
                return None;
            }
            found = Some(path.to_owned());
        }
    }
    found
}

/// Whether `path` opens with a session header naming `id`. A file that cannot be read, or whose
/// first line is not that header, names nothing: a copy or an import can carry another id than its
/// filename, and the header is what Pi itself trusts.
fn header_names(path: &Path, id: &str) -> bool {
    header_of(path).is_some_and(|header| header["id"].as_str() == Some(id))
}

/// The session `path` holds, when it is a Pi session file whose header names a directory. A file
/// that is not one, or whose header names no directory, is left out rather than shown as belonging
/// nowhere.
fn session_of(path: &Path) -> Result<Option<SessionRef>> {
    if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") || !path.is_file() {
        return Ok(None);
    }
    let Some(header) = header_of(path) else {
        return Ok(None);
    };
    let Some(cwd) = header["cwd"]
        .as_str()
        .filter(|cwd| Path::new(cwd).is_absolute())
    else {
        return Ok(None);
    };
    let id = header["id"]
        .as_str()
        .context("Pi session header has no id")?;
    Ok(Some(SessionRef {
        id: id.to_owned(),
        path: path.to_owned(),
        runtime: "pi",
        cwd: Some(cwd.to_owned()),
        mtime: path.metadata()?.modified()?,
        gist: None,
        title: None,
    }))
}

struct Parsed {
    session: Session,
    calls: Vec<Call>,
    outputs: Vec<Output>,
}

/// The conversation in `raw`: a session file Pi wrote, or a published copy of one. Publication
/// leaves the header out, so a copy parses with no id or folder, and showing or resuming it on
/// another computer starts from that copy; requiring a header here would refuse every published
/// session. A file read from disk must still open with one ([`Adapter::parse_at`]).
fn parse(raw: &str) -> Result<Parsed> {
    let rows = rows_of(raw)?;
    let selected = active_lines(&rows)?;
    let mut parsed = Parsed {
        session: Session {
            id: String::new(),
            runtime: "pi".into(),
            cwd: None,
            events: vec![],
        },
        calls: vec![],
        outputs: vec![],
    };
    for (line, value) in &rows {
        let ts = message::timestamp(&value["timestamp"]);
        let mut events = vec![];
        let kind = value["type"].as_str().unwrap_or("");
        if kind == "session" {
            ensure!(
                parsed.session.id.is_empty() && value["version"] == VERSION,
                "unsupported or repeated Pi session header"
            );
            parsed.session.id = value["id"].as_str().context("Pi header has no id")?.into();
            parsed.session.cwd = value["cwd"].as_str().map(str::to_owned);
        } else if !selected.contains(line) {
            events.push(Event::text(EventKind::Other, "", ts.clone()));
        } else if kind == "message" || kind == "custom_message" {
            let body = if kind == "message" {
                &value["message"]
            } else {
                value
            };
            match body["role"].as_str() {
                Some("user" | "assistant") => {
                    let text = message::text(&body["content"]);
                    if !text.is_empty() {
                        events.push(Event::text(
                            if body["role"] == "user" {
                                EventKind::UserPrompt
                            } else {
                                EventKind::AssistantReply
                            },
                            text,
                            ts.clone(),
                        ));
                    }
                    if let Some(blocks) = body["content"].as_array() {
                        for block in blocks {
                            match block["type"].as_str() {
                                Some("toolCall") => {
                                    let call = Call {
                                        line: *line,
                                        id: block["id"]
                                            .as_str()
                                            .context("Pi tool call has no id")?
                                            .into(),
                                        name: block["name"]
                                            .as_str()
                                            .context("Pi tool call has no name")?
                                            .into(),
                                        input: message::input(&block["arguments"]),
                                    };
                                    events.push(message::tool_event(&call, ts.clone()));
                                    parsed.calls.push(call);
                                }
                                Some("text") => {}
                                _ => events.push(Event::text(EventKind::Other, "", ts.clone())),
                            }
                        }
                    }
                    if body["role"] == "assistant" && body["stopReason"] == "stop" {
                        events.push(Event::text(EventKind::TurnEnd, "", ts.clone()));
                    }
                }
                Some("toolResult") => {
                    let output = Output {
                        id: body["toolCallId"]
                            .as_str()
                            .context("Pi tool result has no call id")?
                            .into(),
                        text: message::text(&body["content"]),
                        error: body["isError"] == true,
                    };
                    events.push(Event::text(EventKind::ToolResult, &output.text, ts.clone()));
                    parsed.outputs.push(output);
                }
                _ => events.push(Event::text(EventKind::Other, "", ts.clone())),
            }
        } else if matches!(kind, "compaction" | "branch_summary") {
            events.push(Event::text(
                EventKind::CompactSummary,
                value["summary"].as_str().unwrap_or(""),
                ts.clone(),
            ));
        } else {
            events.push(Event::text(EventKind::Other, "", ts.clone()));
        }
        parsed
            .session
            .events
            .extend(events.into_iter().map(|event| event.at_line(*line)));
    }
    Ok(parsed)
}

/// The records of `raw`, each with the line it stands on. A line that is not JSON is a transcript
/// this build cannot read, not a line to skip: skipping it would move every later line and pair a
/// tool call with another call's result.
fn rows_of(raw: &str) -> Result<Vec<(usize, Value)>> {
    raw.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(line, raw)| {
            let value = serde_json::from_str(raw)
                .with_context(|| format!("Pi transcript line {line} is not JSON"))?;
            Ok((line, value))
        })
        .collect()
}

fn records(raw: &str) -> Result<Vec<Value>> {
    Ok(rows_of(raw)?.into_iter().map(|(_, value)| value).collect())
}

/// The lines that hold the conversation Pi would resume: the records on the path from the root to
/// the leaf, which is the last record in file order. A record Pi wrote off that path (a branch
/// that was left, a label, a compaction of another branch) is not part of it.
fn active_lines(rows: &[(usize, Value)]) -> Result<BTreeSet<usize>> {
    let mut nodes = BTreeMap::<String, (Option<String>, usize)>::new();
    let mut leaf: Option<String> = None;
    for (line, value) in rows {
        if value["type"] == "session" {
            continue;
        }
        let Some(id) = value["id"].as_str() else {
            continue;
        };
        ensure!(
            !nodes.contains_key(id),
            "Pi transcript has duplicate event ids"
        );
        let parent = value["parentId"].as_str().map(str::to_owned);
        nodes.insert(id.to_owned(), (parent, *line));
        leaf = Some(id.to_owned());
    }
    let mut selected = BTreeSet::new();
    let mut cursor = leaf;
    let mut seen = BTreeSet::new();
    while let Some(id) = cursor {
        ensure!(seen.insert(id.clone()), "Pi transcript has a cycle");
        let Some((parent, line)) = nodes.get(&id) else {
            break;
        };
        selected.insert(*line);
        cursor = parent.clone();
    }
    Ok(selected)
}

/// A timestamp Pi accepts, taken from `at` when it is one and otherwise a fixed one. Pi stores
/// whatever string it is given, so the only requirement is that it be stable.
fn stamp(at: Option<&str>) -> String {
    at.unwrap_or("2026-01-01T00:00:00.000Z").to_owned()
}

/// The timestamp part of a session filename, which Pi builds by replacing `:` and `.` in the
/// header's timestamp. A fresh install has no earlier timestamp to keep, so it uses one fixed
/// instant; the session id is what makes the name unique.
fn file_stamp() -> String {
    stamp(None).replace([':', '.'], "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(id: &str, cwd: &str) -> Value {
        json!({"type":"session","version":3,"id":id,"cwd":cwd,"timestamp":"2026-01-01T00:00:00.000Z"})
    }

    #[test]
    fn a_session_parses_its_leaf_path_and_keeps_a_left_branch_out() {
        let raw = format!(
            "{}\n{}\n{}\n{}\n",
            header("s1", "/workspace"),
            json!({"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"Remember the marker"}]}}),
            json!({"type":"message","id":"a","parentId":"u","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"probe.txt"}},{"type":"thinking","thinking":"Opaque native reasoning"}],"stopReason":"toolUse"}}),
            json!({"type":"message","id":"r","parentId":"a","timestamp":"2026-01-01T00:00:03.000Z","message":{"role":"toolResult","toolCallId":"call-1","toolName":"read","content":[{"type":"text","text":"TOOL_MARKER"}],"isError":false}})
        );
        let session = Pi.parse(&raw).unwrap();
        assert_eq!(session.id, "s1");
        assert_eq!(session.cwd.as_deref(), Some("/workspace"));
        assert!(
            session
                .events
                .iter()
                .any(|event| event.text.as_deref() == Some("Remember the marker"))
        );
        assert!(
            session
                .events
                .iter()
                .any(|event| event.kind == EventKind::ToolUse)
        );
        assert!(
            session
                .events
                .iter()
                .any(|event| event.text.as_deref() == Some("TOOL_MARKER"))
        );
        assert!(Pi.open_tool_calls(&raw).is_empty());
        // A branch that was left is not on the path to the leaf, so it is not part of the session.
        let branched = format!(
            "{raw}{}\n",
            json!({"type":"message","id":"side","parentId":"u","timestamp":"2026-01-01T00:00:04.000Z","message":{"role":"assistant","content":[{"type":"text","text":"LEFT_BRANCH"}],"stopReason":"stop"}})
        );
        let session = Pi.parse(&branched).unwrap();
        assert!(
            session
                .events
                .iter()
                .any(|event| event.text.as_deref() == Some("LEFT_BRANCH"))
        );
        let resumed = format!(
            "{branched}{}\n",
            json!({"type":"message","id":"next","parentId":"r","timestamp":"2026-01-01T00:00:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Done"}],"stopReason":"stop"}})
        );
        let session = Pi.parse(&resumed).unwrap();
        assert!(
            !session
                .events
                .iter()
                .any(|event| event.text.as_deref() == Some("LEFT_BRANCH"))
        );
        assert!(
            session
                .events
                .iter()
                .any(|event| event.kind == EventKind::TurnEnd)
        );
    }

    #[test]
    fn an_open_call_is_closed_on_install_and_the_header_is_rewritten() {
        let raw = format!(
            "{}\n{}\n",
            header("s1", "/workspace"),
            json!({"type":"message","id":"a","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"probe.txt"}}],"stopReason":"toolUse"}})
        );
        assert_eq!(Pi.open_tool_calls(&raw).len(), 1);
        let localized = Pi.localize(&raw, "newid1", Path::new("/new")).unwrap();
        assert!(localized.contains("\"cwd\":\"/new\""));
        assert!(localized.contains("newid1"));
        assert!(Pi.open_tool_calls(&localized).is_empty());
        assert_eq!(Pi.parse(&localized).unwrap().id, "newid1");
    }

    #[test]
    fn a_session_id_pi_would_refuse_is_refused() {
        assert!(valid_id("abc123"));
        assert!(valid_id("019a1b2c-3d4e-7f80-9abc-def012345678"));
        assert!(!valid_id(""));
        assert!(!valid_id("-abc"));
        assert!(!valid_id("abc-"));
        assert!(!valid_id("has space"));
        assert!(Pi.localize("{}", "-bad", Path::new("/new")).is_err());
    }

    /// A session installed for a folder lands where Pi keeps that folder's sessions, under the new
    /// id and with the folder in its header, is listed for that folder, is a session file agit
    /// reads, and is resumed by its path. A path outside Pi's sessions folder, one that only names
    /// a session through a link leading out of it, a file not named as a session, and a file
    /// without a Pi header are not session files; a session in the folder
    /// `PI_CODING_AGENT_SESSION_DIR` names is, kept there flat. A check by name alone would read
    /// any file a caller names.
    #[test]
    fn an_installed_session_is_found_where_pi_keeps_it_and_nothing_else_is_read() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let sessions = root.join("agent/sessions");
        let moved = root.join("moved");
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&moved).unwrap();
        let raw = format!(
            "{}\n{}\n{}\n",
            header("old", "/elsewhere"),
            json!({"type":"message","id":"u","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":[{"type":"text","text":"Rename the flag"}]}}),
            json!({"type":"message","id":"a","parentId":"u","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"Renamed."}],"stopReason":"stop"}})
        );
        let id = Pi.mint_id();
        let installed = install_at(&sessions, &raw, &id, &work).unwrap();
        assert_eq!(
            installed.path.parent().unwrap(),
            sessions.join(format!("--{}--", slug(&work)))
        );
        let header = header_of(&installed.path).unwrap();
        assert_eq!(header["id"], id.as_str());
        assert_eq!(header["cwd"], work.to_string_lossy().as_ref());
        let Next::Resume(command) = &installed.next else {
            panic!("an installed session is resumed");
        };
        assert!(
            command.contains(&format!(
                "--session {}",
                crate::adapter::shell_arg(&installed.path.to_string_lossy())
            )),
            "{command}"
        );
        let listed = list_in(&sessions, None).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(
            listed[0].cwd.as_deref(),
            Some(work.to_string_lossy().as_ref())
        );
        let texts: Vec<_> = Pi
            .parse(&std::fs::read_to_string(&installed.path).unwrap())
            .unwrap()
            .events
            .into_iter()
            .filter_map(|event| event.text)
            .filter(|text| !text.is_empty())
            .collect();
        assert_eq!(texts, ["Rename the flag", "Renamed."]);

        let is = |path: &Path| is_session_file_in(Some(&sessions), Some(&moved), path);
        assert!(is(&installed.path));
        let flat = moved.join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        std::fs::copy(&installed.path, &flat).unwrap();
        assert!(is(&flat));
        let outside = root.join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        std::fs::copy(&installed.path, &outside).unwrap();
        assert!(!is(&outside));
        let unnamed = installed.path.with_file_name("notes.jsonl");
        std::fs::copy(&installed.path, &unnamed).unwrap();
        assert!(!is(&unnamed));
        let headless = installed
            .path
            .with_file_name(format!("2026-01-01T00-00-00-000Z_x{id}.jsonl"));
        std::fs::write(&headless, "{\"type\":\"message\"}\n").unwrap();
        assert!(!is(&headless));
        #[cfg(unix)]
        {
            let link = installed
                .path
                .with_file_name(format!("2026-01-01T00-00-00-000Z_y{id}.jsonl"));
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            assert!(!is(&link));
        }
    }

    #[test]
    fn the_project_directory_follows_pi_slug() {
        assert_eq!(
            slug(Path::new("/Users/me/my project")),
            "Users-me-my project"
        );
        assert_eq!(slug(Path::new("C:\\work\\repo")), "C--work-repo");
    }
}
