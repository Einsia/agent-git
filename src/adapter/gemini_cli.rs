//! Gemini CLI's session record.
//!
//! Gemini CLI appends each session to `<home>/tmp/<short id>/chats/session-<time>-<id prefix>.jsonl`,
//! where `<short id>` is what `<home>/projects.json` maps the project root to. The first record
//! holds the session's metadata (`sessionId`, `projectHash`, ...). A message is appended whole,
//! and appended whole again, under the same `id`, whenever it changes: its tokens arrive, or its
//! tool calls are scheduled, run or answered. Other changes are appended as `$patch`, `$rewindTo`
//! and `$set` records that a reader replays in order. Nothing is ever written in place, so every
//! byte offset that once ended a record still does.
//!
//! Each line yields only what is new for its message: the text the first time the message is
//! seen, each tool call the first time its id appears, and each result the first time its call
//! has one. A snapshot that adds nothing yields no conversation event; reading every snapshot
//! whole would repeat the message once per tool step.
//!
//! Only `projects.json` and the `chats/session-*.jsonl` files are read; nothing else under the
//! Gemini home is opened, credentials, histories and logs included.

use super::native_message::{self as message, Call, Output};
use super::{
    Adapter, Capability, Event, EventKind, Installed, Next, OpenCall, Session, SessionRef,
    ToolDetails,
};
use crate::Result;
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

pub struct GeminiCli;

/// The prefix of every main-session file in a project's `chats` folder; a subagent's session sits
/// in a folder below and is not a conversation of its own.
const SESSION_FILE_PREFIX: &str = "session-";

/// How much of a session file is read to find its identity, which its first record holds.
const IDENTITY_READ_LIMIT: u64 = 64 * 1024;

/// Gemini CLI's own folder: `.gemini` under `GEMINI_CLI_HOME` when that is set and not empty, as
/// Gemini resolves it, and under the user's home otherwise.
pub fn home() -> Result<PathBuf> {
    let base = match std::env::var_os("GEMINI_CLI_HOME").filter(|home| !home.is_empty()) {
        Some(home) => PathBuf::from(home),
        None => crate::infra::config::user_home().context("the user home is not set")?,
    };
    Ok(base.join(".gemini"))
}

/// Whether `id` is a full session uuid. Gemini resumes, loads and names a session by it; the
/// eight characters in a file name, a list index or `latest` name a different session as soon as
/// another one is started.
fn validate_id(id: &str) -> Result<()> {
    ensure!(
        id.len() == 36
            && uuid::Uuid::try_parse(id).is_ok()
            && id
                .bytes()
                .all(|byte| byte == b'-' || byte.is_ascii_hexdigit()),
        "invalid Gemini CLI session identity"
    );
    Ok(())
}

/// The project root Gemini works in for `cwd`: the absolute path as given, with symbolic links
/// kept and no trailing separator.
fn project_root(cwd: &Path) -> String {
    let path = std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let root = path
        .to_string_lossy()
        .trim_end_matches(['/', '\\'])
        .to_owned();
    if root.is_empty() {
        "/".to_owned()
    } else {
        root
    }
}

/// The key `projects.json` files `cwd` under: its project root, lowercased on Windows.
fn project_key(cwd: &Path) -> String {
    let root = project_root(cwd);
    if cfg!(windows) {
        root.to_lowercase()
    } else {
        root
    }
}

/// The `projectHash` Gemini records for a session in `cwd`: the SHA-256 of its project root, in
/// lowercase hex (`getProjectHash`). Gemini's reader takes no session whose header lacks one, so
/// every header agit writes carries the one Gemini would write there.
fn project_hash(cwd: &Path) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(project_root(cwd).as_bytes()))
}

/// `projects.json`, read only: each project root and its short id. A missing or unreadable
/// registry maps nothing; agit never writes it.
fn projects(home: &Path) -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(home.join("projects.json")) else {
        return BTreeMap::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return BTreeMap::new();
    };
    value["projects"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(root, short)| {
            let short = short.as_str()?;
            valid_short_id(short).then(|| (root.clone(), short.to_owned()))
        })
        .collect()
}

fn valid_short_id(short: &str) -> bool {
    !short.is_empty()
        && short.len() <= 128
        && short
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// The `chats` folder of the project Gemini files `cwd` under, when it has one.
fn chats_for(home: &Path, cwd: &Path) -> Option<PathBuf> {
    let short = projects(home).remove(&project_key(cwd))?;
    Some(home.join("tmp").join(short).join("chats"))
}

/// The session's identity, from its own records: the metadata record's `sessionId`, as a later
/// `$set` of it leaves it.
pub fn session_id_of(text: &str) -> Option<String> {
    let mut id = None;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(found) = record_session_id(&value) {
            id = Some(found.to_owned());
        }
    }
    id.filter(|id| validate_id(id).is_ok())
}

fn record_session_id(value: &Value) -> Option<&str> {
    if is_metadata(value) {
        return value["sessionId"].as_str();
    }
    value["$set"]["sessionId"].as_str()
}

/// The metadata record a session file opens with: the conversation's fields, no message `id`.
fn is_metadata(value: &Value) -> bool {
    value.get("sessionId").is_some() && value.get("projectHash").is_some()
}

/// The identity a session file opens with, read from its start only.
fn file_session_id(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(std::io::Read::take(file, IDENTITY_READ_LIMIT));
    for line in reader.lines() {
        let line = line.ok()?;
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(id) = record_session_id(&value) {
            return validate_id(id).is_ok().then(|| id.to_owned());
        }
    }
    None
}

/// The transcript of session `id`, filed for the project at `cwd`. Shared by capture and the
/// workspace driver, so both find a session the same way.
pub fn transcript_path(id: &str, cwd: &Path) -> Result<Option<PathBuf>> {
    transcript_path_in(&home()?, id, cwd)
}

fn transcript_path_in(home: &Path, id: &str, cwd: &Path) -> Result<Option<PathBuf>> {
    validate_id(id)?;
    let Some(chats) = chats_for(home, cwd) else {
        return Ok(None);
    };
    Ok(session_files(&chats)
        .into_iter()
        .filter(|path| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem.ends_with(&id[..8]))
        })
        .find(|path| file_session_id(path).as_deref() == Some(id)))
}

/// Whether `path` is a main-session file inside `home`'s `tmp/<short id>/chats/`. A hook names
/// the transcript it reports; anything outside that folder is not a Gemini session and is never
/// read.
pub fn is_session_file(home: &Path, path: &Path) -> bool {
    let (Ok(home), Ok(path)) = (home.join("tmp").canonicalize(), path.canonicalize()) else {
        return false;
    };
    let Ok(relative) = path.strip_prefix(&home) else {
        return false;
    };
    let parts: Vec<_> = relative.components().collect();
    parts.len() == 3
        && parts[1].as_os_str() == "chats"
        && parts[0].as_os_str().to_str().is_some_and(valid_short_id)
        && is_session_name(&path)
}

fn is_session_name(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "jsonl")
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(SESSION_FILE_PREFIX))
}

/// The main-session files of one `chats` folder; the folders below it hold subagent sessions.
fn session_files(chats: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(chats) else {
        return vec![];
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.path())
        .filter(|path| is_session_name(path))
        .collect();
    files.sort();
    files
}

fn list_at(home: &Path, cwd: Option<&Path>) -> Result<Vec<SessionRef>> {
    let registry = projects(home);
    let roots: HashMap<&str, &str> = registry
        .iter()
        .map(|(root, short)| (short.as_str(), root.as_str()))
        .collect();
    let folders: Vec<(PathBuf, Option<String>)> = match cwd {
        Some(cwd) => match registry.get(&project_key(cwd)) {
            Some(short) => vec![(
                home.join("tmp").join(short).join("chats"),
                Some(cwd.to_string_lossy().into_owned()),
            )],
            None => return Ok(vec![]),
        },
        None => {
            let Ok(entries) = std::fs::read_dir(home.join("tmp")) else {
                return Ok(vec![]);
            };
            entries
                .flatten()
                .filter_map(|entry| {
                    let short = entry.file_name().to_str()?.to_owned();
                    valid_short_id(&short).then(|| {
                        (
                            entry.path().join("chats"),
                            roots.get(short.as_str()).map(|root| (*root).to_owned()),
                        )
                    })
                })
                .collect()
        }
    };
    let mut sessions = Vec::new();
    for (chats, cwd) in folders {
        for path in session_files(&chats) {
            let Some(id) = file_session_id(&path) else {
                continue;
            };
            let Ok(metadata) = std::fs::metadata(&path) else {
                // Gemini deletes sessions past its retention; one gone since listing is cold.
                continue;
            };
            sessions.push(SessionRef {
                id,
                path,
                runtime: "gemini-cli",
                cwd: cwd.clone(),
                mtime: metadata.modified()?,
                gist: None,
                title: None,
            });
        }
    }
    Ok(sessions)
}

/// One message as the lines so far have shown it: what has already been read out of it.
#[derive(Default)]
struct Seen {
    text: bool,
    thoughts: usize,
    calls: BTreeSet<String>,
    results: BTreeSet<String>,
}

pub(super) struct Parsed {
    pub session: Session,
    pub calls: Vec<Call>,
    pub outputs: Vec<Output>,
    /// Calls without a result, by id, in the order they appeared.
    pending: Vec<String>,
}

/// The text of Gemini content parts: a string, one part, or a list of parts. Thought parts are
/// reasoning, not text.
pub(crate) fn parts_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part["thought"] != true)
            .filter_map(|part| part.as_str().or_else(|| part["text"].as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) if value["thought"] != true => {
            value["text"].as_str().unwrap_or("").to_owned()
        }
        _ => String::new(),
    }
}

/// The text a tool result's parts hold: each function response's output or error, or the
/// response itself.
pub(crate) fn result_text(value: &Value) -> String {
    let parts: Vec<&Value> = match value {
        Value::Array(parts) => parts.iter().collect(),
        Value::Null => vec![],
        other => vec![other],
    };
    parts
        .into_iter()
        .map(|part| {
            let response = &part["functionResponse"]["response"];
            if let Some(text) = response["output"].as_str().or(response["error"].as_str()) {
                text.to_owned()
            } else if !response.is_null() {
                response.to_string()
            } else {
                parts_text(part)
            }
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a tool call has finished: it has a result, or its status says it ended.
pub(crate) fn call_finished(call: &Value) -> bool {
    !call["result"].is_null()
        || matches!(
            call["status"].as_str(),
            Some("success" | "error" | "cancelled")
        )
}

fn call_failed(call: &Value) -> bool {
    matches!(call["status"].as_str(), Some("error" | "cancelled"))
        || !call["result"]
            .as_array()
            .into_iter()
            .flatten()
            .all(|part| part["functionResponse"]["response"]["error"].is_null())
}

pub(super) fn parse(text: &str) -> Result<Parsed> {
    let mut parsed = Parsed {
        session: Session {
            id: String::new(),
            runtime: "gemini-cli".into(),
            cwd: None,
            events: vec![],
        },
        calls: vec![],
        outputs: vec![],
        pending: vec![],
    };
    let mut seen: HashMap<String, Seen> = HashMap::new();
    let mut lines = text.lines().enumerate().peekable();
    while let Some((line, raw)) = lines.next() {
        if raw.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(error) if error.is_eof() && !text.ends_with('\n') && lines.peek().is_none() => {
                break;
            }
            Err(error) => return Err(error.into()),
        };
        if let Some(id) = record_session_id(&value) {
            ensure!(
                validate_id(id).is_ok(),
                "Gemini CLI session has an invalid identity"
            );
            parsed.session.id = id.to_owned();
        }
        let ts = message::timestamp(&value["timestamp"]);
        let mut events = Vec::new();
        if let Some(patch) = value.get("$patch") {
            // A patch answers calls by id; it can also edit or drop messages, which only the VIEW
            // follows: what was read before stays as it was read.
            let updates: Vec<&Value> = std::iter::once(patch)
                .chain(patch["updates"].as_array().into_iter().flatten())
                .collect();
            for update in updates {
                for call in update["toolCalls"].as_array().into_iter().flatten() {
                    let Some(id) = call["id"].as_str() else {
                        continue;
                    };
                    if call["result"].is_null() || !parsed.calls.iter().any(|c| c.id == id) {
                        continue;
                    }
                    let owner = update["id"].as_str().unwrap_or_default().to_owned();
                    let entry = seen.entry(owner).or_default();
                    if entry.results.insert(id.to_owned())
                        && !parsed.outputs.iter().any(|output| output.id == id)
                    {
                        let output = Output {
                            id: id.to_owned(),
                            text: result_text(&call["result"]),
                            error: call_failed(call),
                        };
                        events.push(Event::text(EventKind::ToolResult, &output.text, ts.clone()));
                        parsed.outputs.push(output);
                        parsed.pending.retain(|pending| pending != id);
                    }
                }
            }
            if events.is_empty() {
                events.push(Event::text(EventKind::Other, "", ts));
            }
        } else if let (Some(id), Some(kind)) = (value["id"].as_str(), value["type"].as_str())
            && !is_metadata(&value)
        {
            let entry = seen.entry(id.to_owned()).or_default();
            let text = parts_text(&value["content"]);
            if !entry.text && !text.is_empty() {
                entry.text = true;
                let event_kind = match kind {
                    "user" => EventKind::UserPrompt,
                    "gemini" => EventKind::AssistantReply,
                    _ => EventKind::Other,
                };
                events.push(Event::text(event_kind, text, ts.clone()));
            }
            let thoughts = value["thoughts"].as_array().map(Vec::len).unwrap_or(0);
            for thought in value["thoughts"]
                .as_array()
                .into_iter()
                .flatten()
                .skip(entry.thoughts)
            {
                let words = thought["description"]
                    .as_str()
                    .or(thought["subject"].as_str())
                    .unwrap_or_default();
                events.push(Event::text(EventKind::Other, words, ts.clone()));
            }
            entry.thoughts = entry.thoughts.max(thoughts);
            if kind == "gemini" {
                for call in value["toolCalls"].as_array().into_iter().flatten() {
                    let Some(call_id) = call["id"].as_str() else {
                        continue;
                    };
                    if entry.calls.insert(call_id.to_owned()) {
                        let native = Call {
                            line,
                            id: call_id.to_owned(),
                            name: call["name"].as_str().unwrap_or("unknown").to_owned(),
                            input: call["args"].clone(),
                        };
                        events.push(message::tool_event(&native, ts.clone()));
                        parsed.calls.push(native);
                        parsed.pending.push(call_id.to_owned());
                    }
                    if call_finished(call)
                        && entry.results.insert(call_id.to_owned())
                        && !parsed.outputs.iter().any(|output| output.id == call_id)
                    {
                        let output = Output {
                            id: call_id.to_owned(),
                            text: result_text(&call["result"]),
                            error: call_failed(call),
                        };
                        events.push(Event::text(EventKind::ToolResult, &output.text, ts.clone()));
                        parsed.outputs.push(output);
                        parsed.pending.retain(|pending| pending != call_id);
                    }
                }
            }
            if events.is_empty() {
                events.push(Event::text(EventKind::Other, "", ts));
            }
        } else {
            events.push(Event::text(EventKind::Other, "", ts));
        }
        parsed
            .session
            .events
            .extend(events.into_iter().map(|event| event.at_line(line)));
    }
    Ok(parsed)
}

/// Every id a record names, minted again from session `session`: a message's id, its tool calls'
/// ids and the function response ids inside their results, the function call and response ids
/// inside its content and `displayContent` parts; a rewind's target; and in a patch the ids of the
/// messages and calls it answers, replaces, removes and orders. One old id always becomes the same
/// new one. A patch left naming the old ids answers nothing, so a call it answered is read, and
/// resumed, as never answered; a content call left with its old id while its result has the new
/// one no longer pairs with it when Gemini rebuilds the model's turn from the content.
fn remint_ids(value: &mut Value, session: &str) {
    fn remint(slot: Option<&mut Value>, session: &str) {
        if let Some(slot) = slot
            && let Some(old) = slot.as_str()
        {
            *slot = json!(message::remint(session, old));
        }
    }
    fn calls(calls: Option<&mut Value>, session: &str) {
        for call in calls.and_then(Value::as_array_mut).into_iter().flatten() {
            remint(call.get_mut("id"), session);
            parts(call.get_mut("result"), session);
        }
    }
    fn parts(parts: Option<&mut Value>, session: &str) {
        let mut each = |part: &mut Value| {
            for kind in ["functionCall", "functionResponse"] {
                remint(
                    part.get_mut(kind).and_then(|call| call.get_mut("id")),
                    session,
                );
            }
        };
        match parts {
            Some(Value::Array(parts)) => parts.iter_mut().for_each(&mut each),
            Some(part @ Value::Object(_)) => each(part),
            _ => {}
        }
    }
    fn record(value: &mut Value, session: &str) {
        remint(value.get_mut("id"), session);
        calls(value.get_mut("toolCalls"), session);
        parts(value.get_mut("content"), session);
        parts(value.get_mut("displayContent"), session);
    }
    record(value, session);
    remint(value.get_mut("$rewindTo"), session);
    if let Some(patch) = value.get_mut("$patch") {
        record(patch, session);
        for update in patch
            .get_mut("updates")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            record(update, session);
        }
        for key in ["removeIds", "orderIds"] {
            for id in patch
                .get_mut(key)
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten()
            {
                remint(Some(id), session);
            }
        }
    }
    for legacy in value
        .get_mut("$set")
        .and_then(|set| set.get_mut("messages"))
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        record(legacy, session);
    }
}

/// When a new session file for this install is named: Gemini's own pattern, the minute it was
/// written and the first eight characters of the session id.
fn file_name(id: &str) -> String {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H-%M");
    format!("{SESSION_FILE_PREFIX}{now}-{}.jsonl", &id[..8])
}

fn install_at(home: &Path, content: &str, id: &str, cwd: &Path) -> Result<PathBuf> {
    validate_id(id)?;
    let chats = chats_for(home, cwd).with_context(|| {
        format!(
            "Gemini CLI has no project for {}; run `gemini` there once, then restore again",
            cwd.display()
        )
    })?;
    let content = GeminiCli.localize(content, id, cwd)?;
    ensure!(
        session_id_of(&content).as_deref() == Some(id),
        "Gemini CLI history has no session identity"
    );
    std::fs::create_dir_all(&chats)?;
    let path = chats.join(file_name(id));
    let mut staged = tempfile::NamedTempFile::new_in(&chats)?;
    staged.write_all(content.as_bytes())?;
    staged.as_file().sync_all()?;
    staged
        .persist_noclobber(&path)
        .map_err(|error| error.error)?;
    Ok(path)
}

impl Adapter for GeminiCli {
    fn id(&self) -> &'static str {
        "gemini-cli"
    }
    fn cli(&self) -> &'static str {
        "gemini"
    }
    fn format(&self) -> &'static str {
        "gemini-cli"
    }
    fn capability(&self) -> Capability {
        Capability::Resumable
    }
    fn available(&self) -> bool {
        super::which("gemini").is_some()
    }
    fn start_command(&self, _cwd: &Path) -> Option<String> {
        super::which("gemini").map(|_| "gemini".into())
    }
    /// `gemini --resume <uuid>`; Gemini finds the session only from the project folder it was
    /// saved in, so the caller runs it there.
    fn resume_command(
        &self,
        id: &str,
        _cwd: &Path,
        prompt: Option<&str>,
        system: Option<&str>,
    ) -> Option<String> {
        if system.is_some() {
            return None;
        }
        validate_id(id).ok()?;
        let mut command = format!("gemini --resume {}", super::shell_arg(id));
        if let Some(prompt) = prompt {
            command.push_str(&format!(
                " --prompt-interactive {}",
                super::shell_arg(prompt)
            ));
        }
        Some(command)
    }
    fn native_files_root(&self) -> Result<PathBuf> {
        Ok(home()?.join("tmp"))
    }
    fn record_identity(&self, value: &Value) -> Option<(String, Option<String>)> {
        Some((record_session_id(value)?.to_owned(), None))
    }
    fn is_runtime_bookkeeping(&self, record: &Value) -> bool {
        is_metadata(record) || record.get("$set").is_some()
    }
    fn sessions_for(&self, repo: &Path) -> Result<Vec<SessionRef>> {
        list_at(&home()?, Some(repo))
    }
    fn all_sessions(&self) -> Result<Vec<SessionRef>> {
        list_at(&home()?, None)
    }
    fn resolve(&self, id: &str, cwd: Option<&Path>) -> Option<PathBuf> {
        validate_id(id).ok()?;
        if let Some(cwd) = cwd
            && let Ok(Some(path)) = transcript_path(id, cwd)
        {
            return Some(path);
        }
        list_at(&home().ok()?, None)
            .ok()?
            .into_iter()
            .find(|session| session.id == id)
            .map(|session| session.path)
    }
    fn parse(&self, text: &str) -> Result<Session> {
        Ok(parse(text)?.session)
    }
    fn tool_details(&self, raw: &str, session: &Session, include_output: bool) -> ToolDetails {
        parse(raw)
            .map(|parsed| message::details(session, &parsed.calls, &parsed.outputs, include_output))
            .unwrap_or_default()
    }
    fn open_tool_calls(&self, raw: &str) -> Vec<OpenCall> {
        let Ok(parsed) = parse(raw) else {
            return vec![];
        };
        parsed
            .calls
            .iter()
            .filter(|call| parsed.pending.contains(&call.id))
            .map(|call| OpenCall {
                line: call.line,
                call_id: call.id.clone(),
                record: "gemini".into(),
                name: call.name.clone(),
            })
            .collect()
    }
    fn mint_id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
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
        validate_id(id)?;
        let now = chrono::Utc::now().to_rfc3339();
        let mut out = format!(
            "{}\n",
            json!({"sessionId":id,"projectHash":project_hash(cwd),"startTime":now,"lastUpdated":now,"kind":"main"})
        );
        let mut reply: Option<Value> = None;
        let flush = |reply: &mut Option<Value>, out: &mut String| {
            if let Some(value) = reply.take() {
                out.push_str(&format!("{value}\n"));
            }
        };
        for (index, event) in session.events.iter().enumerate() {
            let key = message::remint(id, &index.to_string());
            let timestamp = event.timestamp.clone().unwrap_or_else(|| now.clone());
            match event.kind {
                EventKind::UserPrompt
                | EventKind::UserInterjection
                | EventKind::CompactSummary
                | EventKind::CompactFiltered => {
                    flush(&mut reply, &mut out);
                    out.push_str(&format!(
                        "{}\n",
                        json!({"id":key,"timestamp":timestamp,"type":"user","content":[{"text":event.text.as_deref().unwrap_or("")}]})
                    ));
                }
                EventKind::AssistantReply => {
                    flush(&mut reply, &mut out);
                    reply = Some(
                        json!({"id":key,"timestamp":timestamp,"type":"gemini","content":[{"text":event.text.as_deref().unwrap_or("")}]}),
                    );
                }
                EventKind::ToolUse | EventKind::FileEdit if !details.is_receipt(index) => {
                    let detail = details.get(index).cloned().unwrap_or_default();
                    let name = event.tool.as_deref().unwrap_or("unknown");
                    let output = detail
                        .output
                        .as_deref()
                        .unwrap_or(crate::domain::install::OPEN_CALL_PLACEHOLDER_OUTPUT);
                    let call = json!({
                        "id":key, "name":name, "args":detail.input.unwrap_or(json!({})),
                        "status": if detail.error { "error" } else { "success" },
                        "timestamp":timestamp,
                        "result":[{"functionResponse":{"id":key,"name":name,"response":{"output":output}}}],
                    });
                    let holder = reply.get_or_insert_with(|| {
                        json!({"id":message::remint(id, &format!("calls:{index}")),"timestamp":timestamp,"type":"gemini","content":""})
                    });
                    match holder["toolCalls"].as_array_mut() {
                        Some(calls) => calls.push(call),
                        None => holder["toolCalls"] = json!([call]),
                    }
                }
                _ => {}
            }
        }
        flush(&mut reply, &mut out);
        Ok(out)
    }
    /// The same history under session `id`: the identity records name `id`, and message and
    /// call ids are minted again from it, so the copy never shares an id with the session it
    /// came from. Calls still open are closed, as a resumed session needs every call answered.
    fn localize(&self, content: &str, id: &str, cwd: &Path) -> Result<String> {
        validate_id(id)?;
        let mut out = String::new();
        let mut identified = false;
        for raw in content.lines().filter(|line| !line.trim().is_empty()) {
            let mut value: Value = serde_json::from_str(raw)?;
            ensure!(value.is_object(), "Gemini CLI record is not an object");
            if is_metadata(&value) {
                value["sessionId"] = json!(id);
                value["projectHash"] = json!(project_hash(cwd));
                identified = true;
            } else if value["$set"].get("sessionId").is_some() {
                value["$set"]["sessionId"] = json!(id);
            }
            remint_ids(&mut value, id);
            out.push_str(&format!("{value}\n"));
        }
        if !identified {
            let now = chrono::Utc::now().to_rfc3339();
            out.insert_str(
                0,
                &format!(
                    "{}\n",
                    json!({"sessionId":id,"projectHash":project_hash(cwd),"startTime":now,"lastUpdated":now,"kind":"main"})
                ),
            );
        }
        let open = self.open_tool_calls(&out);
        if !open.is_empty() {
            let mut closing = BTreeMap::<String, Vec<String>>::new();
            for line in out.lines() {
                let value: Value = serde_json::from_str(line)?;
                if value["type"] == "gemini" {
                    for call in value["toolCalls"].as_array().into_iter().flatten() {
                        if let (Some(message), Some(call)) =
                            (value["id"].as_str(), call["id"].as_str())
                            && open.iter().any(|open| open.call_id == call)
                        {
                            closing
                                .entry(message.to_owned())
                                .or_default()
                                .push(call.to_owned());
                        }
                    }
                }
            }
            for (message_id, calls) in closing {
                let updates: Vec<Value> = calls
                    .into_iter()
                    .map(|call| json!({"id":call, "result":[{"functionResponse":{"id":call,"response":{"error":crate::domain::install::OPEN_CALL_PLACEHOLDER_OUTPUT}}}]}))
                    .collect();
                out.push_str(&format!(
                    "{}\n",
                    json!({"$patch":{"id":message_id,"toolCalls":updates}})
                ));
            }
        }
        Ok(out)
    }
    fn install(&self, content: &str, id: &str, cwd: &Path) -> Result<Installed> {
        let command = self
            .resume_command(id, cwd, None, None)
            .context("install Gemini CLI before restoring a session")?;
        let path = install_at(&home()?, content, id, cwd)?;
        Ok(Installed {
            path,
            next: Next::Resume(format!(
                "(cd {} && {command})",
                super::shell_arg(&cwd.to_string_lossy())
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "0b4c7f7e-3f43-4d8e-9a59-1f0c2b8f4a10";

    fn record(value: Value) -> String {
        format!("{value}\n")
    }

    /// A session as Gemini writes it: the reply is appended once with its call scheduled, again
    /// with the call answered, and again with its tokens; a second call is answered by a patch.
    fn transcript() -> Vec<String> {
        let call = |id: &str, status: &str, result: Value| json!({"id":id,"name":"read_file","args":{"file_path":"notes.md"},"status":status,"result":result,"timestamp":"2026-11-13T10:00:01Z"});
        let answered = json!([{"functionResponse":{"id":"c1","name":"read_file","response":{"output":"NOTES"}}}]);
        let reply = |calls: Value, tokens: Value| json!({"id":"m2","timestamp":"2026-11-13T10:00:01Z","type":"gemini","content":[{"text":"Reading it"}],"thoughts":[{"subject":"Plan","description":"read the notes","timestamp":"t"}],"toolCalls":calls,"tokens":tokens,"model":"gemini-x"});
        vec![
            record(
                json!({"sessionId":SESSION,"projectHash":"p","startTime":"t","lastUpdated":"t","kind":"main"}),
            ),
            record(
                json!({"id":"m1","timestamp":"2026-11-13T10:00:00Z","type":"user","content":[{"text":"Read notes.md"}]}),
            ),
            record(json!({"$set":{"lastUpdated":"t"}})),
            record(reply(
                json!([call("c1", "scheduled", Value::Null)]),
                Value::Null,
            )),
            record(reply(
                json!([call("c1", "success", answered.clone())]),
                Value::Null,
            )),
            record(reply(
                json!([
                    call("c1", "success", answered.clone()),
                    call("c2", "executing", Value::Null)
                ]),
                json!({"input":1,"output":2,"cached":0,"total":3}),
            )),
            record(
                json!({"$patch":{"updates":[{"id":"m2","toolCalls":[{"id":"c2","result":[{"functionResponse":{"id":"c2","name":"read_file","response":{"output":"MORE"}}}]}]}]}}),
            ),
            record(
                json!({"id":"m3","timestamp":"2026-11-13T10:00:02Z","type":"gemini","content":[{"text":"Done"}]}),
            ),
        ]
    }

    /// Each line yields only what is new for its message: one reply, one reasoning entry, each
    /// call once and each result once, though the reply is written three times and its second
    /// call is answered by a patch. A call is open until its result arrives, and only then can a
    /// turn end past it. Reading each snapshot whole repeats the reply and its calls per tool
    /// step; reading only the last one loses the results that arrived by patch.
    #[test]
    fn repeated_snapshots_yield_each_part_once_and_calls_stay_open_until_answered() {
        let lines = transcript();
        let text = lines.concat();
        let parsed = parse(&text).unwrap();
        assert_eq!(parsed.session.id, SESSION);
        let count = |kind: EventKind| {
            parsed
                .session
                .events
                .iter()
                .filter(|event| event.kind == kind)
                .count()
        };
        assert_eq!(count(EventKind::UserPrompt), 1);
        assert_eq!(count(EventKind::AssistantReply), 2);
        assert_eq!(count(EventKind::ToolUse), 2);
        assert_eq!(count(EventKind::ToolResult), 2);
        assert_eq!(
            parsed
                .outputs
                .iter()
                .map(|output| output.text.as_str())
                .collect::<Vec<_>>(),
            ["NOTES", "MORE"]
        );
        assert!(GeminiCli.open_tool_calls(&text).is_empty());
        let scheduled = lines[..4].concat();
        assert_eq!(GeminiCli.open_tool_calls(&scheduled).len(), 1);
        let second_open = lines[..6].concat();
        assert_eq!(
            GeminiCli
                .open_tool_calls(&second_open)
                .into_iter()
                .map(|call| call.call_id)
                .collect::<Vec<_>>(),
            ["c2"]
        );
    }

    /// Sessions are found through `projects.json` alone and named by their full uuid; a
    /// subagent's session in the folder below is not one of them, and a path outside a project's
    /// `chats` folder is never a session. A saved history installs only into a folder Gemini
    /// already files, under the new id, and reads back as the same conversation.
    #[test]
    fn sessions_are_found_through_the_registry_and_install_only_where_gemini_files_the_folder() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".gemini");
        let project = root.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(home.join("tmp/project/chats/sub")).unwrap();
        std::fs::write(
            home.join("projects.json"),
            json!({"projects":{project_key(&project):"project"}}).to_string(),
        )
        .unwrap();
        let chats = home.join("tmp/project/chats");
        let main = chats.join(format!("session-2026-11-13T10-00-{}.jsonl", &SESSION[..8]));
        std::fs::write(&main, transcript().concat()).unwrap();
        let sub = chats.join("sub").join(format!("{SESSION}.jsonl"));
        std::fs::write(&sub, transcript().concat()).unwrap();

        let listed = list_at(&home, Some(&project)).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, SESSION);
        assert_eq!(listed[0].path, main);
        assert_eq!(list_at(&home, None).unwrap().len(), 1);
        assert_eq!(
            transcript_path_in(&home, SESSION, &project).unwrap(),
            Some(main.clone())
        );
        assert!(transcript_path_in(&home, &SESSION[..8], &project).is_err());
        assert!(is_session_file(&home, &main));
        assert!(!is_session_file(&home, &sub));
        let outside = root.path().join("session-x.jsonl");
        std::fs::write(&outside, "").unwrap();
        assert!(!is_session_file(&home, &outside));

        let saved = GeminiCli.parse(&transcript().concat()).unwrap();
        let id = GeminiCli.mint_id();
        let rendered = GeminiCli.render(&saved, &id, &project).unwrap();
        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        assert!(install_at(&home, &rendered, &id, &elsewhere).is_err());
        let installed = install_at(&home, &rendered, &id, &project).unwrap();
        assert_eq!(installed.parent(), Some(chats.as_path()));
        let back = GeminiCli
            .parse(&std::fs::read_to_string(&installed).unwrap())
            .unwrap();
        assert_eq!(back.id, id);
        let texts = |session: &Session| {
            session
                .events
                .iter()
                .filter(|event| {
                    matches!(
                        event.kind,
                        EventKind::UserPrompt | EventKind::AssistantReply
                    )
                })
                .map(|event| event.text.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(&back), texts(&saved));
    }

    /// A history installed into a project is one Gemini's own reader takes, and finds answered.
    /// Its header carries the `projectHash` Gemini computes for the folder, the SHA-256 of its
    /// root in hex; Gemini's reader refuses a header without one, so `gemini --resume` can't find
    /// the session. Every id a later patch, a removal or an ordering names is minted again with the
    /// message and call it names; a patch still naming the old ids answers nothing, so its call
    /// replays unanswered and its output is lost. The function call and response ids inside a
    /// message's content, and inside content a patch replaces, follow the same mapping: Gemini
    /// rebuilds a model turn's calls from its content and their responses from its tool calls, so a
    /// content call left with its old id pairs with no response.
    #[test]
    fn an_installed_history_is_read_back_by_gemini_answered_and_in_its_project() {
        #[cfg(unix)]
        assert_eq!(
            project_hash(Path::new("/work/app/")),
            "70467eff2e0a236497f0693901c782c674520c598f57e72b22e636e00311bd81"
        );
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join(".gemini");
        let project = root.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("projects.json"),
            json!({"projects":{project_key(&project):"project"}}).to_string(),
        )
        .unwrap();
        let answer = json!([{"functionResponse":{"id":"c1","name":"read_file","response":{"output":"PATCHED"}}}]);
        let history = [
            json!({"sessionId":SESSION,"projectHash":"","startTime":"t","lastUpdated":"t","kind":"main"}),
            json!({"id":"m1","timestamp":"t","type":"user","content":[{"text":"Read it"}]}),
            json!({"id":"m2","timestamp":"t","type":"gemini","content":[{"text":"Reading"}],
                "toolCalls":[{"id":"c1","name":"read_file","args":{"file_path":"a"},"status":"executing"}]}),
            json!({"id":"m3","timestamp":"t","type":"gemini","content":[{"text":"aside"}]}),
            json!({"$patch":{"updates":[{"id":"m2","toolCalls":[{"id":"c1","result":answer}]}],
                "removeIds":["m3"],"orderIds":["m1","m2"]}}),
            // A reply whose content holds its call, as Gemini records one with function calls.
            json!({"id":"m4","timestamp":"t","type":"gemini",
                "content":[{"text":"Checking"},{"functionCall":{"id":"c2","name":"read_file","args":{"file_path":"b"}}}],
                "toolCalls":[{"id":"c2","name":"read_file","args":{"file_path":"b"},"status":"success",
                    "result":[{"functionResponse":{"id":"c2","name":"read_file","response":{"output":"SECOND"}}}]}]}),
            json!({"$patch":{"updates":[{"id":"m4",
                "content":[{"text":"Checked"},{"functionCall":{"id":"c2","name":"read_file","args":{"file_path":"b"}}}]}]}}),
            json!({"id":"m5","timestamp":"t","type":"user",
                "content":[{"functionResponse":{"id":"c2","name":"read_file","response":{"output":"SECOND"}}}]}),
        ]
        .iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>();
        let id = GeminiCli.mint_id();
        let installed =
            std::fs::read_to_string(install_at(&home, &history, &id, &project).unwrap()).unwrap();

        let records: Vec<Value> = installed
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let expected_hash = {
            use sha2::{Digest, Sha256};
            let root = std::path::absolute(&project).unwrap();
            format!("{:x}", Sha256::digest(root.to_string_lossy().as_bytes()))
        };
        assert_eq!(records[0]["sessionId"], id);
        assert_eq!(records[0]["projectHash"], expected_hash);
        for old in ["m1", "m2", "m3", "m4", "m5", "c1", "c2"] {
            assert!(
                !installed.contains(&format!("\"{old}\"")),
                "{old} survives: {installed}"
            );
        }

        // Our reader: the call is answered with its output, so nothing is closed for it.
        let parsed = parse(&installed).unwrap();
        assert!(parsed.pending.is_empty());
        assert_eq!(parsed.outputs.len(), 2);
        assert_eq!(parsed.outputs[0].text, "PATCHED");
        assert_eq!(parsed.outputs[1].text, "SECOND");

        // Gemini's reader, as its loader replays a file: messages by id, then each patch's
        // updates, removals and order.
        let mut messages: Vec<Value> = Vec::new();
        for record in &records {
            if let Some(patch) = record.get("$patch") {
                for update in patch["updates"].as_array().into_iter().flatten() {
                    let Some(message) = messages.iter_mut().find(|m| m["id"] == update["id"])
                    else {
                        continue;
                    };
                    if let Some(content) = update.get("content") {
                        message["content"] = content.clone();
                    }
                    for answer in update["toolCalls"].as_array().into_iter().flatten() {
                        if let Some(call) = message["toolCalls"]
                            .as_array_mut()
                            .into_iter()
                            .flatten()
                            .find(|call| call["id"] == answer["id"])
                        {
                            call["result"] = answer["result"].clone();
                        }
                    }
                }
                for removed in patch["removeIds"].as_array().into_iter().flatten() {
                    messages.retain(|m| m["id"] != *removed);
                }
                for ordered in patch["orderIds"].as_array().into_iter().flatten() {
                    assert!(messages.iter().any(|m| m["id"] == *ordered), "{ordered}");
                }
            } else if record["id"].is_string() && record["type"].is_string() {
                match messages.iter_mut().find(|m| m["id"] == record["id"]) {
                    Some(message) => *message = record.clone(),
                    None => messages.push(record.clone()),
                }
            }
        }
        assert_eq!(messages.len(), 4, "the removed message stays removed");
        let call = &messages[1]["toolCalls"][0];
        let response = &call["result"][0]["functionResponse"];
        assert_eq!(response["response"]["output"], "PATCHED");
        assert_eq!(response["id"], call["id"]);
        // Gemini's converter: a reply whose content holds calls takes its calls from the content
        // and their responses from its tool calls' results; every call pairs with a response.
        let ids = |parts: &Value, kind: &str| -> Vec<Value> {
            parts
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part.get(kind).map(|part| part["id"].clone()))
                .collect()
        };
        let reply = &messages[2];
        let called = ids(&reply["content"], "functionCall");
        let answered: Vec<Value> = reply["toolCalls"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|call| ids(&call["result"], "functionResponse"))
            .collect();
        assert_eq!(called.len(), 1);
        assert_eq!(called, answered);
        assert_eq!(called, ids(&messages[3]["content"], "functionResponse"));
    }
}
