//! OpenClaw transcripts are ordered events in the selected agent's native SQLite store.

use super::native_message::{self as message, Call, Output};
use super::native_snapshot::{self as native, Limits, Snapshot, Source, Unavailable};
use super::sqlite_native as db;
use super::{
    Adapter, Capability, Event, EventKind, Installed, Next, OpenCall, Session, SessionRef,
    ToolDetails,
};
use crate::Result;
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, UNIX_EPOCH},
};

pub struct OpenClaw;

pub fn home() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("OPENCLAW_STATE_DIR").filter(|root| !root.is_empty()) {
        return Ok(root.into());
    }
    Ok(crate::infra::config::user_home()
        .context("the user home is not set")?
        .join(".openclaw"))
}

fn databases(root: &Path, limits: Limits) -> Result<Vec<PathBuf>> {
    let agents = root.join("agents");
    if !agents.exists() {
        return Ok(vec![]);
    }
    let mut found = vec![];
    for (count, entry) in std::fs::read_dir(agents)?.enumerate() {
        ensure!(
            count < limits.lookup_entries,
            "OpenClaw agent lookup budget exceeded"
        );
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let path = entry.path().join("agent/openclaw-agent.sqlite");
            if path.exists() {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

fn read_at(source: &Source, limits: Limits) -> Result<Snapshot> {
    ensure!(
        source.runtime == "openclaw" && source.database,
        "not an OpenClaw database source"
    );
    let mut conn = db::open(&source.path, limits)?;
    let tx = conn.transaction()?;
    let mut bytes = vec![];
    {
        let mut stmt = tx.prepare(
            "SELECT event_json FROM transcript_events WHERE session_id = ? ORDER BY seq",
        )?;
        let mut rows = stmt.query([&source.session_id])?;
        let mut count = 0;
        while let Some(row) = rows.next()? {
            let raw = row.get_ref(0)?.as_str()?;
            count += 1;
            ensure!(
                count <= limits.records
                    && bytes.len().saturating_add(raw.len()).saturating_add(1)
                        <= limits.bytes.min(limits.working_bytes / 8),
                "OpenClaw snapshot budget exceeded"
            );
            ensure!(
                !raw.contains(['\n', '\r']),
                "OpenClaw transcript event is not a single JSON record"
            );
            bytes.extend_from_slice(raw.as_bytes());
            bytes.push(b'\n');
        }
    }
    ensure!(
        !bytes.is_empty(),
        "OpenClaw session has no transcript header"
    );
    let first = bytes.split(|byte| *byte == b'\n').next().unwrap();
    let header: Value = serde_json::from_slice(first)?;
    ensure!(
        header["type"] == "session" && header["id"] == source.session_id,
        "OpenClaw transcript identity differs from its database window"
    );
    tx.commit()?;
    Ok(native::finish(source.clone(), bytes, limits)?)
}

fn list_at(root: &Path, cwd: Option<&Path>) -> Result<Vec<SessionRef>> {
    let mut sessions = vec![];
    let limits = Limits::default();
    for path in databases(root, limits)? {
        let conn = db::open(&path, limits)?;
        let mut stmt = conn.prepare("SELECT w.session_id, COALESCE(w.transcript_updated_at,w.updated_at), json_extract(e.event_json,'$.cwd'), w.display_name FROM session_windows w JOIN transcript_events e ON e.session_id=w.session_id AND e.seq=(SELECT min(first.seq) FROM transcript_events first WHERE first.session_id=w.session_id) WHERE (?1 IS NULL OR json_extract(e.event_json,'$.cwd')=?1) ORDER BY w.updated_at DESC LIMIT ?2")?;
        let wanted = cwd.map(|path| {
            path.canonicalize()
                .unwrap_or_else(|_| path.into())
                .to_string_lossy()
                .into_owned()
        });
        let mut rows = stmt.query(rusqlite::params![wanted, limits.lookup_entries as i64 + 1])?;
        while let Some(row) = rows.next()? {
            ensure!(
                sessions.len() < limits.lookup_entries,
                "OpenClaw session lookup budget exceeded"
            );
            let id: String = row.get(0)?;
            let updated: i64 = row.get(1)?;
            sessions.push(SessionRef {
                title: None,
                path: db::cache_path("openclaw", &id)?,
                id,
                runtime: "openclaw",
                cwd: row.get(2)?,
                mtime: UNIX_EPOCH + Duration::from_millis(updated.max(0) as u64),
                gist: row.get(3)?,
            });
        }
    }
    Ok(sessions)
}

fn records(raw: &str) -> Result<Vec<Value>> {
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn canonical(kind: &str) -> bool {
    matches!(
        kind,
        "message"
            | "thinking_level_change"
            | "model_change"
            | "compaction"
            | "reset"
            | "branch_summary"
            | "custom"
            | "custom_message"
            | "label"
            | "session_info"
    )
}

fn active_lines(rows: &[(usize, Value)]) -> Result<BTreeSet<usize>> {
    let mut nodes = BTreeMap::<String, (Option<String>, usize)>::new();
    let mut leaf: Option<String> = None;
    let mut append_parent: Option<String> = None;
    let mut reset_descendants = BTreeSet::new();
    let mut reset_seen = false;
    let mut controls = BTreeSet::new();
    for (line, value) in rows {
        let kind = value["type"].as_str().unwrap_or("");
        if kind == "session" {
            continue;
        }
        let Some(id) = value["id"].as_str() else {
            continue;
        };
        ensure!(
            !nodes.contains_key(id),
            "OpenClaw transcript has duplicate event ids"
        );
        let mut parent = if kind == "leaf" {
            let target = &value["targetId"];
            ensure!(
                target.is_null()
                    || target
                        .as_str()
                        .is_some_and(|target| nodes.contains_key(target)),
                "OpenClaw leaf references an unavailable event"
            );
            target.as_str().map(str::to_owned)
        } else if value.get("parentId").is_none() {
            leaf.clone()
        } else {
            value["parentId"].as_str().map(str::to_owned)
        };
        if kind == "leaf"
            && reset_seen
            && parent
                .as_ref()
                .is_none_or(|id| !reset_descendants.contains(id))
        {
            parent = leaf.clone();
        }
        if canonical(kind) && value["appendMode"] != "side" {
            if (reset_seen
                && parent
                    .as_ref()
                    .is_none_or(|id| !reset_descendants.contains(id)))
                || (parent.as_ref().is_some_and(|id| !nodes.contains_key(id)) && leaf.is_some())
                || (parent == append_parent && leaf != append_parent)
            {
                parent = leaf.clone();
            }
            while let Some(control) = parent.as_ref().filter(|id| controls.contains(*id)) {
                parent = nodes.get(control).and_then(|(parent, _)| parent.clone());
            }
        }
        if kind == "reset" {
            reset_seen = true;
            reset_descendants.clear();
            reset_descendants.insert(id.to_owned());
        } else if parent
            .as_ref()
            .is_some_and(|id| reset_descendants.contains(id))
        {
            reset_descendants.insert(id.to_owned());
        }
        nodes.insert(id.into(), (parent.clone(), *line));
        append_parent = if kind == "leaf" {
            controls.insert(id.to_owned());
            value
                .get("appendParentId")
                .map(|v| v.as_str().map(str::to_owned))
                .unwrap_or_else(|| parent.clone())
        } else {
            Some(id.to_owned())
        };
        if kind == "leaf" {
            leaf = parent;
        } else if canonical(kind) && value["appendMode"] != "side" {
            leaf = Some(id.into());
        }
    }
    let mut selected = BTreeSet::new();
    let mut seen = BTreeSet::new();
    while let Some(id) = leaf {
        ensure!(
            seen.insert(id.clone()),
            "OpenClaw transcript contains a parent cycle"
        );
        let Some((parent, line)) = nodes.get(&id) else {
            break;
        };
        selected.insert(*line);
        leaf = parent.clone();
    }
    Ok(selected)
}

struct Parsed {
    session: Session,
    calls: Vec<Call>,
    outputs: Vec<Output>,
}
fn parse(raw: &str) -> Result<Parsed> {
    let rows: Vec<(usize, Value)> = raw
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| Ok((i, serde_json::from_str(line)?)))
        .collect::<Result<_>>()?;
    let selected = active_lines(&rows)?;
    let mut parsed = Parsed {
        session: Session {
            id: String::new(),
            runtime: "openclaw".into(),
            cwd: None,
            events: vec![],
        },
        calls: vec![],
        outputs: vec![],
    };
    for (line, value) in rows {
        let ts = message::timestamp(&value["timestamp"]);
        let mut events = vec![];
        let kind = value["type"].as_str().unwrap_or("");
        if kind == "session" {
            ensure!(
                parsed.session.id.is_empty() && matches!(value["version"].as_u64(), Some(3 | 4)),
                "unsupported or repeated OpenClaw session header"
            );
            parsed.session.id = value["id"]
                .as_str()
                .context("OpenClaw header has no id")?
                .into();
            parsed.session.cwd = value["cwd"].as_str().map(str::to_owned);
        } else if !selected.contains(&line) {
            events.push(Event::text(EventKind::Other, "", ts.clone()));
        } else if kind == "message" || kind == "custom_message" {
            let body = if kind == "message" {
                &value["message"]
            } else {
                &value
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
                                        line,
                                        id: block["id"]
                                            .as_str()
                                            .context("OpenClaw tool call has no id")?
                                            .into(),
                                        name: block["name"]
                                            .as_str()
                                            .context("OpenClaw tool call has no name")?
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
                            .context("OpenClaw tool result has no call id")?
                            .into(),
                        text: message::text(&body["content"]),
                        error: body["isError"] == true,
                    };
                    events.push(Event::text(EventKind::ToolResult, &output.text, ts.clone()));
                    parsed.outputs.push(output);
                }
                _ => events.push(Event::text(EventKind::Other, "", ts.clone())),
            }
        } else if matches!(kind, "compaction" | "branch_summary" | "reset") {
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
            .extend(events.into_iter().map(|event| event.at_line(line)));
    }
    // Native snapshot reads validate the database window against its header. An archived
    // fragment can omit that header; its native identity then remains unknown in the IR.
    Ok(parsed)
}

fn installation() -> Option<(PathBuf, PathBuf)> {
    let mut roots = vec![];
    if let Some(root) = std::env::var_os("OPENCLAW_PACKAGE_ROOT") {
        roots.push(PathBuf::from(root));
    }
    if let Some(binary) = super::which("openclaw").and_then(|path| path.canonicalize().ok())
        && let Some(parent) = binary.parent()
    {
        roots.push(parent.into());
        roots.push(parent.join("../lib/node_modules/openclaw"));
    }
    if let Some(home) = crate::infra::config::user_home() {
        roots.push(home.join(".local/share/openclaw-runtime/node_modules/openclaw"));
    }
    let root = roots
        .into_iter()
        .find(|root| root.join("package.json").is_file() && root.join("openclaw.mjs").is_file())?
        .canonicalize()
        .ok()?;
    let bundled = root.parent()?.join("node/bin/node");
    let node = if bundled.is_file() {
        bundled
    } else {
        super::which("node")?
    };
    Some((root, node))
}

fn launch_bridge() -> Result<String> {
    use sha2::Digest;
    let (root, node) = installation().context("cannot locate the OpenClaw installation")?;
    let code = include_str!("openclaw/launch.mjs");
    let directory = crate::infra::config::agit_home()?.join("cache/native-launch");
    std::fs::create_dir_all(&directory)?;
    let path = directory.canonicalize()?.join(format!(
        "openclaw-{:x}.mjs",
        sha2::Sha256::digest(code.as_bytes())
    ));
    if !path.exists() {
        let mut staged = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        staged.write_all(code.as_bytes())?;
        if let Err(error) = staged.persist_noclobber(&path)
            && error.error.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(error.error.into());
        }
    }
    ensure!(
        path.symlink_metadata()?.is_file() && std::fs::read(&path)? == code.as_bytes(),
        "OpenClaw launch helper does not match this AgentGit build"
    );
    Ok(format!(
        "{} {} --root {}",
        super::shell_arg(&node.to_string_lossy()),
        super::shell_arg(&path.to_string_lossy()),
        super::shell_arg(&root.to_string_lossy())
    ))
}

fn prompt_command(
    bridge: &str,
    id: &str,
    cwd: &Path,
    agent: Option<&str>,
    prompt: Option<&str>,
) -> String {
    let mut command = format!(
        "OPENCLAW_SESSION_ID={} {bridge} --session-id {} --cwd {}",
        super::shell_arg(id),
        super::shell_arg(id),
        super::shell_arg(&cwd.to_string_lossy())
    );
    if let Some(agent) = agent {
        command.push_str(&format!(" --agent {}", super::shell_arg(agent)));
    }
    if let Some(prompt) = prompt {
        return format!("{command} --message {}", super::shell_arg(prompt));
    }
    format!(
        "while printf 'You: ' && IFS= read -r agit_openclaw_prompt; do [ -z \"$agit_openclaw_prompt\" ] || {command} --message \"$agit_openclaw_prompt\" || exit $?; done"
    )
}

fn local_command(id: &str, cwd: &Path, prompt: Option<&str>) -> Result<String> {
    let cwd = std::path::absolute(cwd)?;
    let source = OpenClaw.lookup_native_readonly(id, Limits::default()).ok();
    let agent = source
        .as_ref()
        .and_then(|source| source.path.parent()?.parent()?.file_name()?.to_str());
    Ok(prompt_command(&launch_bridge()?, id, &cwd, agent, prompt))
}

impl Adapter for OpenClaw {
    fn id(&self) -> &'static str {
        "openclaw"
    }
    fn cli(&self) -> &'static str {
        "openclaw"
    }
    fn format(&self) -> &'static str {
        "openclaw"
    }
    fn capability(&self) -> Capability {
        Capability::Resumable
    }
    fn start_command(&self, cwd: &Path) -> Option<String> {
        local_command(&self.mint_id(), cwd, None).ok()
    }
    fn resume_command(
        &self,
        id: &str,
        cwd: &Path,
        prompt: Option<&str>,
        system: Option<&str>,
    ) -> Option<String> {
        native::validate_id(id, Limits::default()).ok()?;
        if system.is_some() {
            return None;
        }
        local_command(id, cwd, prompt).ok()
    }
    fn requires_turn_end(&self) -> bool {
        true
    }
    fn tail_is_complete(&self, events: &[&Event]) -> bool {
        events
            .iter()
            .rev()
            .find(|e| e.kind != EventKind::Other)
            .is_some_and(|e| e.kind == EventKind::TurnEnd)
    }
    fn sessions_for(&self, cwd: &Path) -> Result<Vec<SessionRef>> {
        list_at(&home()?, Some(cwd))
    }
    fn all_sessions(&self) -> Result<Vec<SessionRef>> {
        list_at(&home()?, None)
    }
    fn lookup_native_readonly(&self, id: &str, limits: Limits) -> native::Result<Source> {
        native::validate_id(id, limits)?;
        let mut selected = None;
        for path in databases(&home().map_err(|_| Unavailable::Read)?, limits)
            .map_err(|_| Unavailable::Database)?
        {
            let conn = db::open(&path, limits).map_err(|_| Unavailable::Database)?;
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM session_windows WHERE session_id=?",
                    [id],
                    |row| row.get(0),
                )
                .map_err(|_| Unavailable::Database)?;
            if count > 0 {
                if selected.is_some() || count != 1 {
                    return Err(Unavailable::Ambiguous);
                }
                selected = Some(db::source("openclaw", id, path, limits)?);
            }
        }
        selected.ok_or(Unavailable::NotFound)
    }
    fn snapshot_native_readonly(
        &self,
        source: &Source,
        limits: Limits,
    ) -> native::Result<Snapshot> {
        read_at(source, limits).map_err(|error| {
            error
                .downcast_ref::<Unavailable>()
                .copied()
                .unwrap_or(Unavailable::Database)
        })
    }
    fn resolve(&self, id: &str, _cwd: Option<&Path>) -> Option<PathBuf> {
        db::cache(
            self.snapshot_native_readonly(
                &self.lookup_native_readonly(id, Limits::default()).ok()?,
                Limits::default(),
            )
            .ok()?,
        )
        .ok()
    }
    fn read_native_bytes_at(&self, id: &str, _path: &Path) -> Result<Vec<u8>> {
        Ok(self
            .snapshot_native_readonly(
                &self.lookup_native_readonly(id, Limits::default())?,
                Limits::default(),
            )?
            .bytes)
    }
    fn parse(&self, raw: &str) -> Result<Session> {
        Ok(parse(raw)?.session)
    }
    fn parse_at(&self, path: &Path) -> Result<Session> {
        if !path.exists()
            && let Some(id) = path.file_stem().and_then(|s| s.to_str())
        {
            self.resolve(id, None)
                .context("OpenClaw session not found")?;
        }
        self.parse(&std::fs::read_to_string(path)?)
    }
    fn record_identity(&self, value: &Value) -> Option<(String, Option<String>)> {
        (value["type"] == "session").then(|| {
            Some((
                value["id"].as_str()?.into(),
                value["cwd"].as_str().map(str::to_owned),
            ))
        })?
    }
    fn line_details(&self, value: &Value) -> super::detail::LineDetails {
        use super::detail::{EventDetail, LineDetails, plaintext, summary};
        let mut details = LineDetails::default();
        if matches!(
            value["type"].as_str(),
            Some("compaction" | "branch_summary" | "reset")
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
        let rows = raw
            .lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(line, raw)| serde_json::from_str(raw).map(|value| (line, value)))
            .collect::<std::result::Result<Vec<_>, _>>();
        let selected = rows.ok().and_then(|rows| active_lines(&rows).ok());
        super::detail::project(self, raw, session, |line| {
            selected.as_ref().is_some_and(|lines| lines.contains(&line))
        })
    }

    fn tool_details(&self, raw: &str, session: &Session, output: bool) -> ToolDetails {
        parse(raw)
            .map(|p| message::details(session, &p.calls, &p.outputs, output))
            .unwrap_or_default()
    }
    fn open_tool_calls(&self, raw: &str) -> Vec<OpenCall> {
        parse(raw)
            .map(|p| message::open_calls(&p.calls, &p.outputs, "toolCall"))
            .unwrap_or_default()
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
        let mut out = format!(
            "{}\n",
            json!({"type":"session","version":4,"id":id,"cwd":cwd.to_string_lossy(),"timestamp":"2026-01-01T00:00:00.000Z"})
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
                    value["message"]["content"] = json!([{"type":"toolCall","id":key,"name":event.tool.as_deref().unwrap_or("unknown"),"arguments":detail.input.unwrap_or(json!({}))}]);
                    out.push_str(&format!("{value}\n"));
                    value["id"] = json!(message::remint(id, &format!("result:{index}")));
                    value["parentId"] = json!(key);
                    value["message"] = json!({"role":"toolResult","toolCallId":key,"toolName":event.tool,"content":[{"type":"text","text":detail.output.as_deref().unwrap_or(crate::domain::install::OPEN_CALL_PLACEHOLDER_OUTPUT)}],"isError":detail.error,"timestamp":message::millis(Some(ts))});
                }
                _ => continue,
            }
            parent = value["id"].clone();
            out.push_str(&format!("{value}\n"));
        }
        Ok(out)
    }
    fn localize(&self, raw: &str, id: &str, cwd: &Path) -> Result<String> {
        native::validate_id(id, Limits::default())?;
        let parsed = parse(raw)?;
        let mut rows = records(raw)?;
        if rows.first().is_none_or(|value| value["type"] != "session") {
            rows.insert(0, json!({"type":"session","version":4,"id":id,"cwd":cwd.to_string_lossy(),"timestamp":"2026-01-01T00:00:00.000Z"}));
        }
        for value in &mut rows {
            if value["type"] == "session" {
                value["id"] = json!(id);
                value["cwd"] = json!(cwd.to_string_lossy());
                value.as_object_mut().unwrap().remove("parentSession");
            }
            // Event and call identities are local to the fresh transcript window.
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
            rows.push(json!({"type":"message","id":key,"parentId":parent,"timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"toolResult","toolCallId":call.call_id,"toolName":call.name,"content":[{"type":"text","text":crate::domain::install::OPEN_CALL_PLACEHOLDER_OUTPUT}],"isError":true,"timestamp":0}}));
            parent = json!(key);
        }
        Ok(rows.into_iter().map(|v| format!("{v}\n")).collect())
    }
    fn install(&self, raw: &str, id: &str, cwd: &Path) -> Result<Installed> {
        let content = self.localize(raw, id, cwd)?;
        let (root, node) = installation()
            .context("cannot locate the installed OpenClaw package and Node runtime")?;
        let mut child = Command::new(node)
            .args([
                "--input-type=module",
                "-e",
                include_str!("openclaw/install.mjs"),
            ])
            .arg(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .context("OpenClaw installer stdin is unavailable")?
            .write_all(
                serde_json::to_string(
                    &json!({"content":content,"id":id,"cwd":cwd.to_string_lossy()}),
                )?
                .as_bytes(),
            )?;
        let output = child.wait_with_output()?;
        ensure!(
            output.status.success(),
            "OpenClaw native installation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let path = self
            .resolve(id, Some(cwd))
            .context("OpenClaw did not expose the installed session")?;
        Ok(Installed {
            path,
            next: Next::Resume(format!(
                "(cd {} && {})",
                super::shell_arg(&cwd.to_string_lossy()),
                local_command(id, cwd, None)?
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw() -> String {
        [json!({"type":"session","version":4,"id":"session-1","cwd":"/workspace"}),
         json!({"type":"message","id":"u","parentId":null,"message":{"role":"user","content":[{"type":"text","text":"Remember the marker"}]}}),
         json!({"type":"message","id":"a","parentId":"u","message":{"role":"assistant","content":[{"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"probe.txt"}},{"type":"thinking","thinking":"Opaque native reasoning"}],"stopReason":"toolUse"}}),
         json!({"type":"message","id":"r","parentId":"a","message":{"role":"toolResult","toolCallId":"call-1","toolName":"read","content":[{"type":"text","text":"TOOL_MARKER"}]}}),
         json!({"type":"message","id":"z","parentId":"r","message":{"role":"assistant","content":[{"type":"text","text":"Done"}],"stopReason":"stop"},"future":{"preserved":true}}),
        ].into_iter().map(|row|format!("{row}\n")).collect()
    }

    #[test]
    fn materialized_fragments_keep_coordinates_without_inventing_native_identity() {
        let fragment = raw()
            .lines()
            .skip(1)
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        let session = OpenClaw.parse(&fragment).unwrap();
        assert!(session.id.is_empty());
        assert_eq!(session.events.first().unwrap().line, Some(0));
        let call = session
            .events
            .iter()
            .position(|event| event.kind == EventKind::ToolUse)
            .unwrap();
        assert_eq!(
            OpenClaw
                .tool_details(&fragment, &session, true)
                .get(call)
                .unwrap()
                .output
                .as_deref(),
            Some("TOOL_MARKER")
        );
        let restored = OpenClaw
            .localize(&fragment, "restored", Path::new("/workspace"))
            .unwrap();
        let header: Value = serde_json::from_str(restored.lines().next().unwrap()).unwrap();
        assert_eq!(header["type"], "session");
        assert_eq!(OpenClaw.parse(&restored).unwrap().id, "restored");
    }

    #[test]
    fn native_calls_outputs_and_opaque_fields_survive_localization() {
        let raw = raw();
        let session = OpenClaw.parse(&raw).unwrap();
        assert_eq!(session.events.last().unwrap().kind, EventKind::TurnEnd);
        let call = session
            .events
            .iter()
            .position(|e| e.kind == EventKind::ToolUse)
            .unwrap();
        assert_eq!(
            OpenClaw
                .tool_details(&raw, &session, true)
                .get(call)
                .unwrap()
                .output
                .as_deref(),
            Some("TOOL_MARKER")
        );
        let details = OpenClaw.event_details(&raw, &session);
        assert_eq!(
            details[call].as_ref().unwrap().input.as_ref().unwrap()["path"],
            "probe.txt"
        );
        assert!(
            details
                .iter()
                .flatten()
                .any(|detail| detail.text.as_deref() == Some("Opaque native reasoning"))
        );
        assert!(OpenClaw.open_tool_calls(&raw).is_empty());
        let localized = OpenClaw
            .localize(&raw, "new-id", Path::new("/new"))
            .unwrap();
        assert!(localized.contains("Opaque native reasoning"));
        assert!(localized.contains("future"));
        assert_eq!(OpenClaw.parse(&localized).unwrap().id, "new-id");
        assert_eq!(super::super::infer_runtime(&raw), Some("openclaw"));
    }

    #[test]
    fn selecting_an_old_leaf_does_not_replay_the_abandoned_branch() {
        let mut raw = raw();
        raw.push_str(&format!(
            "{}\n",
            json!({"type":"leaf","id":"rewind","parentId":"z","targetId":"u"})
        ));
        raw.push_str(&format!("{}\n",json!({"type":"message","id":"alternate","parentId":"rewind","message":{"role":"assistant","content":[{"type":"text","text":"Alternate response"}],"stopReason":"stop"}})));
        let session = OpenClaw.parse(&raw).unwrap();
        assert!(!session.events.iter().any(|e| e.kind == EventKind::ToolUse));
        assert!(
            OpenClaw
                .event_details(&raw, &session)
                .iter()
                .all(Option::is_none)
        );
        assert!(
            session
                .events
                .iter()
                .any(|e| e.text.as_deref() == Some("Alternate response"))
        );
        assert!(
            !session
                .events
                .iter()
                .any(|e| e.text.as_deref() == Some("Done"))
        );
        assert!(
            OpenClaw
                .parse(&raw.replace("\"targetId\":\"u\"", "\"targetId\":\"missing\""))
                .is_err()
        );
    }

    #[test]
    fn snapshot_keeps_native_event_bytes_and_observes_wal_appends() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("openclaw-agent.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE transcript_events(session_id TEXT,seq INTEGER,event_json TEXT);").unwrap();
        for (seq, line) in raw().lines().enumerate() {
            conn.execute(
                "INSERT INTO transcript_events VALUES(?1,?2,?3)",
                rusqlite::params!["session-1", seq as i64, line],
            )
            .unwrap();
        }
        let source = db::source("openclaw", "session-1", path, Limits::default()).unwrap();
        let before = read_at(&source, Limits::default()).unwrap();
        assert_eq!(before.bytes, raw().as_bytes());
        conn.execute("INSERT INTO transcript_events VALUES(?1,?2,?3)",rusqlite::params!["session-1",5,"{\"type\":\"custom\",\"id\":\"tail\",\"parentId\":\"z\",\"data\":{\"opaque\":true}} "]).unwrap();
        assert!(
            read_at(&source, Limits::default())
                .unwrap()
                .bytes
                .starts_with(&before.bytes)
        );
        assert!(
            read_at(
                &source,
                Limits {
                    records: 1,
                    ..Limits::default()
                }
            )
            .is_err()
        );
        let wrong = Source {
            session_id: "foreign".into(),
            ..source
        };
        assert!(read_at(&wrong, Limits::default()).is_err());
    }

    #[test]
    fn side_appends_do_not_enter_the_visible_continuation() {
        let mut raw = raw();
        raw.push_str(&format!("{}\n", json!({"type":"message","id":"side","parentId":"z","appendMode":"side","message":{"role":"user","content":[{"type":"text","text":"Side-only input"}]}})));
        raw.push_str(&format!("{}\n", json!({"type":"message","id":"next","parentId":"side","message":{"role":"user","content":[{"type":"text","text":"Continue"}]}})));
        let session = OpenClaw.parse(&raw).unwrap();
        assert!(
            !session
                .events
                .iter()
                .any(|e| e.text.as_deref() == Some("Side-only input"))
        );
        assert!(
            session
                .events
                .iter()
                .any(|e| e.text.as_deref() == Some("Continue"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_prompt_loop_keeps_input_literal_and_session_identity_fixed() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("openclaw");
        std::fs::write(&executable, "#!/bin/sh\nprintf '%s\\0' \"$@\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut child = Command::new("/bin/sh")
            .args([
                "-c",
                &prompt_command(
                    "openclaw",
                    "native-id",
                    Path::new("/workspace '$(false)"),
                    Some("agent ' name"),
                    None,
                ),
            ])
            .env("PATH", root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"literal $HOME `false` ' quote\nsecond turn\n")
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();
        assert!(output.contains("literal $HOME `false` ' quote"));
        assert_eq!(output.matches("/workspace '$(false)").count(), 2);
        assert_eq!(output.matches("agent ' name").count(), 2);
        assert_eq!(output.matches("native-id").count(), 2);
    }
}
