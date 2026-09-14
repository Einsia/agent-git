//! Hermes message rows retain native state while the IR projects the active conversation.

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
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, UNIX_EPOCH},
};

pub struct Hermes;

pub fn home() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("HERMES_HOME").filter(|root| !root.is_empty()) {
        return Ok(root.into());
    }
    Ok(crate::infra::config::user_home()
        .context("the user home is not set")?
        .join(".hermes"))
}

fn records(text: &str) -> Result<Vec<Value>> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn read_at(source: &Source, limits: Limits) -> Result<Snapshot> {
    ensure!(
        source.runtime == "hermes" && source.database,
        "not a Hermes database source"
    );
    let mut conn = db::open(&source.path, limits)?;
    let tx = conn.transaction()?;
    let header = {
        let mut stmt = tx.prepare(
            "SELECT id, source, cwd, started_at, parent_session_id FROM sessions WHERE id = ?",
        )?;
        let mut rows = stmt.query([&source.session_id])?;
        let header = db::row(rows.next()?.context("Hermes session not found")?, limits)?;
        ensure!(rows.next()?.is_none(), "ambiguous Hermes session identity");
        header
    };
    let mut bytes = vec![];
    let mut count = 0;
    db::push(
        &mut bytes,
        &json!({"type":"hermes_session","version":1,"data":header}),
        &mut count,
        limits,
    )?;
    {
        let mut stmt = tx.prepare("SELECT * FROM messages WHERE session_id = ? ORDER BY id")?;
        let mut rows = stmt.query([&source.session_id])?;
        while let Some(row) = rows.next()? {
            db::push(
                &mut bytes,
                &json!({"type":"hermes_message","data":db::row(row, limits)?}),
                &mut count,
                limits,
            )?;
        }
    }
    tx.commit()?;
    Ok(native::finish(source.clone(), bytes, limits)?)
}

fn list_at(path: &Path, cwd: Option<&Path>) -> Result<Vec<SessionRef>> {
    if !path.exists() {
        return Ok(vec![]);
    }
    let limits = Limits::default();
    let conn = db::open(path, limits)?;
    let mut stmt = conn.prepare("SELECT id, cwd, COALESCE(last_activity_at, ended_at, started_at), title FROM sessions WHERE (?1 IS NULL OR cwd = ?1) ORDER BY COALESCE(last_activity_at, ended_at, started_at) DESC LIMIT ?2")?;
    let wanted = cwd.map(|path| {
        path.canonicalize()
            .unwrap_or_else(|_| path.into())
            .to_string_lossy()
            .into_owned()
    });
    let mut rows = stmt.query(rusqlite::params![wanted, limits.lookup_entries as i64 + 1])?;
    let mut result = vec![];
    while let Some(row) = rows.next()? {
        ensure!(
            result.len() < limits.lookup_entries,
            "Hermes session lookup budget exceeded"
        );
        let id: String = row.get(0)?;
        let started: f64 = row.get(2)?;
        ensure!(
            started.is_finite() && started <= 253402300799.0,
            "invalid Hermes session timestamp"
        );
        result.push(SessionRef {
            path: db::cache_path("hermes", &id)?,
            id,
            runtime: "hermes",
            cwd: row.get(1)?,
            mtime: UNIX_EPOCH + Duration::from_secs_f64(started.max(0.0)),
            gist: row.get(3)?,
        });
    }
    Ok(result)
}

fn content(value: &Value) -> Value {
    value
        .as_str()
        .and_then(|s| s.strip_prefix("\0json:"))
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_else(|| value.clone())
}

struct Parsed {
    session: Session,
    calls: Vec<Call>,
    outputs: Vec<Output>,
}
fn parse(raw: &str) -> Result<Parsed> {
    let mut parsed = Parsed {
        session: Session {
            id: String::new(),
            runtime: "hermes".into(),
            cwd: None,
            events: vec![],
        },
        calls: vec![],
        outputs: vec![],
    };
    let mut seen_header = false;
    for (line, raw) in raw.lines().enumerate() {
        if raw.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(raw)?;
        let data = &value["data"];
        match value["type"].as_str() {
            Some("hermes_session") => {
                ensure!(
                    !seen_header && parsed.session.id.is_empty() && value["version"] == 1,
                    "invalid Hermes snapshot header"
                );
                seen_header = true;
                parsed.session.id = data["id"]
                    .as_str()
                    .context("Hermes header has no identity")?
                    .into();
                parsed.session.cwd = data["cwd"].as_str().map(str::to_owned);
            }
            Some("hermes_message") => {
                let id = data["session_id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .context("Hermes message has no session identity")?;
                // Archived VIEWs and materialized increments may start after the native header.
                if parsed.session.id.is_empty() {
                    parsed.session.id = id.into();
                }
                ensure!(
                    id == parsed.session.id,
                    "Hermes snapshot mixes session identities"
                );
                let ts = data["timestamp"]
                    .as_f64()
                    .and_then(|seconds| message::timestamp(&json!((seconds * 1000.0) as i64)));
                let mut events = vec![];
                if data["active"].as_i64().unwrap_or(1) == 0 {
                    events.push(Event::text(EventKind::Other, "", ts.clone()));
                } else {
                    let body = message::text(&content(&data["content"]));
                    match data["role"].as_str() {
                        Some("user" | "assistant" | "system") => {
                            let kind = if data["_compressed_summary"] == 1 {
                                EventKind::CompactSummary
                            } else if data["role"] == "user" {
                                EventKind::UserPrompt
                            } else if data["role"] == "assistant" {
                                EventKind::AssistantReply
                            } else {
                                EventKind::Other
                            };
                            if !body.is_empty() {
                                events.push(Event::text(kind, body, ts.clone()));
                            }
                            if let Some(calls) = message::input(&data["tool_calls"]).as_array() {
                                for call in calls {
                                    let call = Call {
                                        line,
                                        id: call["id"]
                                            .as_str()
                                            .context("Hermes tool call has no id")?
                                            .into(),
                                        name: call["function"]["name"]
                                            .as_str()
                                            .context("Hermes tool call has no name")?
                                            .into(),
                                        input: message::input(&call["function"]["arguments"]),
                                    };
                                    events.push(message::tool_event(&call, ts.clone()));
                                    parsed.calls.push(call);
                                }
                            }
                            if data["role"] == "assistant" && data["finish_reason"] == "stop" {
                                events.push(Event::text(EventKind::TurnEnd, "", ts.clone()));
                            }
                            if [
                                "reasoning",
                                "reasoning_content",
                                "reasoning_details",
                                "codex_reasoning_items",
                            ]
                            .iter()
                            .any(|key| !data[*key].is_null())
                            {
                                events.push(Event::text(EventKind::Other, "", ts.clone()));
                            }
                        }
                        Some("tool") => {
                            parsed.outputs.push(Output {
                                id: data["tool_call_id"]
                                    .as_str()
                                    .context("Hermes tool result has no call id")?
                                    .into(),
                                text: body.clone(),
                                error: data["effect_disposition"] == "error",
                            });
                            events.push(Event::text(EventKind::ToolResult, body, ts.clone()));
                        }
                        _ => events.push(Event::text(EventKind::Other, "", ts.clone())),
                    }
                }
                parsed
                    .session
                    .events
                    .extend(events.into_iter().map(|event| event.at_line(line)));
            }
            _ => anyhow::bail!("unrecognized Hermes snapshot record"),
        }
    }
    Ok(parsed)
}

pub(crate) fn python() -> Option<PathBuf> {
    let binary = super::which("hermes")?.canonicalize().ok()?;
    let path = binary.parent()?.join(if cfg!(windows) {
        "python.exe"
    } else {
        "python"
    });
    path.is_file().then_some(path)
}

impl Adapter for Hermes {
    fn id(&self) -> &'static str {
        "hermes"
    }
    fn cli(&self) -> &'static str {
        "hermes"
    }
    fn format(&self) -> &'static str {
        "hermes"
    }
    fn capability(&self) -> Capability {
        Capability::Resumable
    }
    fn start_command(&self, _cwd: &Path) -> Option<String> {
        Some("hermes".into())
    }
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
        native::validate_id(id, Limits::default()).ok()?;
        let mut command = format!(
            "hermes chat --resume {} --no-restore-cwd",
            super::shell_arg(id)
        );
        if let Some(prompt) = prompt {
            command.push_str(&format!(" --query {}", super::shell_arg(prompt)));
        }
        Some(command)
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
        list_at(&home()?.join("state.db"), Some(cwd))
    }
    fn all_sessions(&self) -> Result<Vec<SessionRef>> {
        list_at(&home()?.join("state.db"), None)
    }
    fn lookup_native_readonly(&self, id: &str, limits: Limits) -> native::Result<Source> {
        let source = db::source(
            "hermes",
            id,
            home().map_err(|_| Unavailable::Read)?.join("state.db"),
            limits,
        )?;
        let conn = db::open(&source.path, limits).map_err(|_| Unavailable::Database)?;
        let count: i64 = conn
            .query_row("SELECT count(*) FROM sessions WHERE id = ?", [id], |row| {
                row.get(0)
            })
            .map_err(|_| Unavailable::Database)?;
        match count {
            1 => Ok(source),
            0 => Err(Unavailable::NotFound),
            _ => Err(Unavailable::Ambiguous),
        }
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
        let source = self.lookup_native_readonly(id, Limits::default()).ok()?;
        db::cache(
            self.snapshot_native_readonly(&source, Limits::default())
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
        if let Some(id) = path.file_stem().and_then(|s| s.to_str())
            && !path.exists()
        {
            self.resolve(id, None).context("Hermes session not found")?;
        }
        self.parse(&std::fs::read_to_string(path)?)
    }
    fn line_details(&self, value: &Value) -> super::detail::LineDetails {
        use super::detail::{EventDetail, LineDetails, plaintext, summary};
        let mut details = LineDetails::default();
        let data = &value["data"];
        if value["type"] != "hermes_message" || data["active"] == 0 {
            return details;
        }
        if data["_compressed_summary"] == 1 {
            details.compact = Some(summary(&message::text(&content(&data["content"]))));
        } else if data["role"] == "system" && !message::text(&content(&data["content"])).is_empty()
        {
            details.opaque.push(EventDetail {
                reason: Some("injected_by_runtime"),
                ..plaintext(&message::text(&content(&data["content"])))
            });
        }
        if matches!(data["role"].as_str(), Some("user" | "assistant" | "system")) {
            if let Some(calls) = message::input(&data["tool_calls"]).as_array() {
                details.tools.extend(calls.iter().map(|call| EventDetail {
                    input: Some(message::input(&call["function"]["arguments"])),
                    ..Default::default()
                }));
            }
            let body = ["reasoning", "reasoning_content"]
                .iter()
                .filter_map(|key| data[*key].as_str())
                .collect::<Vec<_>>()
                .join(
                    "
",
                );
            if !body.is_empty() {
                details.opaque.push(plaintext(&body));
            } else if [
                "reasoning",
                "reasoning_content",
                "reasoning_details",
                "codex_reasoning_items",
            ]
            .iter()
            .any(|key| !data[*key].is_null())
            {
                details.opaque.push(EventDetail {
                    reason: Some("native_reasoning"),
                    ..Default::default()
                });
            }
        }
        details
    }

    fn tool_details(&self, raw: &str, session: &Session, output: bool) -> ToolDetails {
        parse(raw)
            .map(|p| message::details(session, &p.calls, &p.outputs, output))
            .unwrap_or_default()
    }
    fn open_tool_calls(&self, raw: &str) -> Vec<OpenCall> {
        parse(raw)
            .map(|p| message::open_calls(&p.calls, &p.outputs, "tool_calls"))
            .unwrap_or_default()
    }
    fn record_group(&self, value: &Value) -> Option<String> {
        self.record_identity(value)
            .map(|(id, _)| id)
            .filter(|id| !id.is_empty())
    }
    fn record_identity(&self, value: &Value) -> Option<(String, Option<String>)> {
        match value["type"].as_str()? {
            "hermes_session" => Some((
                value["data"]["id"].as_str()?.into(),
                value["data"]["cwd"].as_str().map(str::to_owned),
            )),
            "hermes_message" => Some((value["data"]["session_id"].as_str()?.into(), None)),
            _ => None,
        }
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
        let mut rows = vec![
            json!({"type":"hermes_session","version":1,"data":{"id":id,"cwd":cwd.to_string_lossy(),"source":"cli","started_at":0}}),
        ];
        for (index, event) in session.events.iter().enumerate() {
            let mut row = json!({"session_id":id,"timestamp":message::millis(event.timestamp.as_deref()) as f64 / 1000.0,"active":1,"compacted":0});
            match event.kind {
                EventKind::UserPrompt
                | EventKind::UserInterjection
                | EventKind::AssistantReply
                | EventKind::CompactSummary
                | EventKind::CompactFiltered => {
                    row["role"] = json!(if event.kind == EventKind::AssistantReply {
                        "assistant"
                    } else {
                        "user"
                    });
                    row["content"] = json!(event.text.as_deref().unwrap_or(""));
                    if event.kind == EventKind::AssistantReply {
                        row["finish_reason"] = json!("stop");
                    }
                    if event.kind.is_compact() {
                        row["_compressed_summary"] = json!(1);
                    }
                }
                EventKind::ToolUse | EventKind::FileEdit if !details.is_receipt(index) => {
                    let detail = details.get(index).cloned().unwrap_or_default();
                    let call = message::remint(id, &index.to_string());
                    row["role"] = json!("assistant");
                    row["finish_reason"] = json!("tool_calls");
                    row["tool_calls"] = json!(json!([{"id":call,"type":"function","function":{"name":event.tool.as_deref().unwrap_or("unknown"),"arguments":detail.input.unwrap_or(json!({})).to_string()}}]).to_string());
                    rows.push(json!({"type":"hermes_message","data":row}));
                    row.as_object_mut().unwrap().remove("tool_calls");
                    row.as_object_mut().unwrap().remove("finish_reason");
                    row["role"] = json!("tool");
                    row["tool_call_id"] = json!(call);
                    row["tool_name"] = json!(event.tool);
                    row["content"] = json!(
                        detail
                            .output
                            .as_deref()
                            .unwrap_or(crate::domain::install::OPEN_CALL_PLACEHOLDER_OUTPUT)
                    );
                }
                _ => continue,
            }
            rows.push(json!({"type":"hermes_message","data":row}));
        }
        Ok(rows.into_iter().map(|value| format!("{value}\n")).collect())
    }
    fn localize(&self, raw: &str, id: &str, cwd: &Path) -> Result<String> {
        native::validate_id(id, Limits::default())?;
        self.parse(raw)?;
        let mut values = records(raw)?;
        if values
            .first()
            .is_none_or(|value| value["type"] != "hermes_session")
        {
            values.insert(0, json!({"type":"hermes_session","version":1,"data":{"id":id,"cwd":cwd.to_string_lossy(),"source":"cli","started_at":0}}));
        }
        let mut out = String::new();
        for mut value in values {
            if value["type"] == "hermes_session" {
                value["data"]["id"] = json!(id);
                value["data"]["cwd"] = json!(cwd.to_string_lossy());
                value["data"]["parent_session_id"] = Value::Null;
            } else {
                value["data"]["session_id"] = json!(id);
                // Tool identifiers are scoped to the fresh native session, so their pairing survives unchanged.
            }
            out.push_str(&format!("{value}\n"));
        }
        for call in self.open_tool_calls(&out) {
            out.push_str(&format!("{}\n", json!({"type":"hermes_message","data":{"session_id":id,"role":"tool","tool_call_id":call.call_id,"tool_name":call.name,"content":crate::domain::install::OPEN_CALL_PLACEHOLDER_OUTPUT,"active":1,"compacted":0,"timestamp":0}})));
        }
        Ok(out)
    }
    fn install(&self, raw: &str, id: &str, cwd: &Path) -> Result<Installed> {
        let content = self.localize(raw, id, cwd)?;
        let python =
            python().context("cannot find the Python environment of the installed Hermes CLI")?;
        let mut child = Command::new(python)
            .args(["-c", include_str!("hermes/install.py")])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .context("Hermes installer stdin is unavailable")?
            .write_all(
                serde_json::to_string(
                    &json!({"content":content,"id":id,"cwd":cwd.to_string_lossy()}),
                )?
                .as_bytes(),
            )?;
        let output = child.wait_with_output()?;
        ensure!(
            output.status.success(),
            "Hermes native installation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let path = self
            .resolve(id, Some(cwd))
            .context("Hermes did not expose the installed session")?;
        Ok(Installed {
            path,
            next: Next::Resume(format!(
                "(cd {} && {})",
                super::shell_arg(&cwd.to_string_lossy()),
                self.resume_command(id, cwd, None, None).unwrap()
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Value {
        json!({"type":"hermes_session","version":1,"data":{"id":"session-1","cwd":"/workspace","started_at":0}})
    }

    fn transcript() -> String {
        [header(),
         json!({"type":"hermes_message","data":{"id":1,"session_id":"session-1","role":"user","content":"Remember the original marker","active":0,"compacted":1}}),
         json!({"type":"hermes_message","data":{"id":2,"session_id":"session-1","role":"user","content":"The compacted context","active":1,"_compressed_summary":1}}),
         json!({"type":"hermes_message","data":{"id":3,"session_id":"session-1","role":"assistant","active":1,"tool_calls":"[{\"id\":\"call-1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"probe.txt\\\"}\"}}]","reasoning_content":"Preserved native reasoning","finish_reason":"tool_calls"}}),
         json!({"type":"hermes_message","data":{"id":4,"session_id":"session-1","role":"tool","tool_call_id":"call-1","content":"TOOL_MARKER","active":1}}),
         json!({"type":"hermes_message","data":{"id":5,"session_id":"session-1","role":"assistant","content":"Done","active":1,"finish_reason":"stop","future_field":{"preserved":true}}}),
        ].into_iter().map(|v| format!("{v}\n")).collect()
    }

    #[test]
    fn materialized_fragments_keep_identity_and_restore_a_native_header() {
        let fragment = transcript()
            .lines()
            .skip(1)
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        let session = Hermes.parse(&fragment).unwrap();
        assert_eq!(session.id, "session-1");
        assert!(
            session
                .events
                .iter()
                .any(|event| event.kind == EventKind::ToolUse)
        );
        assert!(!Hermes.tool_details(&fragment, &session, true).is_empty());
        let restored = Hermes
            .localize(&fragment, "restored", Path::new("/workspace"))
            .unwrap();
        let header: Value = serde_json::from_str(restored.lines().next().unwrap()).unwrap();
        assert_eq!(header["type"], "hermes_session");
        assert_eq!(Hermes.parse(&restored).unwrap().id, "restored");
        let mixed = fragment
            + "{\"type\":\"hermes_message\",\"data\":{\"session_id\":\"another\",\"role\":\"user\",\"content\":\"Unrelated\"}}\n";
        assert!(Hermes.parse(&mixed).is_err());
        assert!(
            Hermes
                .parse(r#"{"type":"hermes_message","data":{"role":"user"}}"#)
                .is_err()
        );
    }

    #[test]
    fn active_projection_keeps_compaction_tool_pairing_and_completion() {
        let raw = transcript();
        let parsed = Hermes.parse(&raw).unwrap();
        assert!(
            !parsed
                .events
                .iter()
                .any(|e| e.kind == EventKind::UserPrompt)
        );
        assert!(
            parsed
                .events
                .iter()
                .any(|e| e.kind == EventKind::CompactSummary)
        );
        assert_eq!(parsed.events.last().unwrap().kind, EventKind::TurnEnd);
        let tool = parsed
            .events
            .iter()
            .position(|e| e.kind == EventKind::ToolUse)
            .unwrap();
        let details = Hermes.tool_details(&raw, &parsed, true);
        assert_eq!(
            details.get(tool).unwrap().output.as_deref(),
            Some("TOOL_MARKER")
        );
        assert!(Hermes.open_tool_calls(&raw).is_empty());
        let local = Hermes.localize(&raw, "new-id", Path::new("/new")).unwrap();
        assert!(local.contains("Preserved native reasoning"));
        assert!(local.contains("Remember the original marker"));
        assert!(local.contains("future_field"));
        assert_eq!(Hermes.parse(&local).unwrap().id, "new-id");
        assert_eq!(super::super::infer_runtime(&raw), Some("hermes"));
    }

    #[test]
    fn database_snapshot_is_stable_across_appends_and_preserves_row_mutations() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("state.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE sessions(id TEXT PRIMARY KEY, source TEXT, cwd TEXT, started_at REAL, model TEXT, model_config TEXT, system_prompt TEXT, system_prompt_hash TEXT, parent_session_id TEXT); CREATE TABLE messages(id INTEGER PRIMARY KEY, session_id TEXT, role TEXT, content TEXT, active INTEGER, compacted INTEGER, timestamp REAL, opaque BLOB); INSERT INTO sessions(id,cwd,started_at) VALUES('session-1','/workspace',1); INSERT INTO messages VALUES(1,'session-1','user','Original marker',1,0,1,X'00ff');").unwrap();
        let source = db::source("hermes", "session-1", path, Limits::default()).unwrap();
        let before = read_at(&source, Limits::default()).unwrap();
        conn.execute(
            "INSERT INTO messages VALUES(2,'session-1','assistant','Done',1,0,2,NULL)",
            [],
        )
        .unwrap();
        let appended = read_at(&source, Limits::default()).unwrap();
        assert!(appended.bytes.starts_with(&before.bytes));
        assert!(
            String::from_utf8(before.bytes.clone())
                .unwrap()
                .contains("$sqlite_blob_hex")
        );
        conn.execute("UPDATE messages SET active=0,compacted=1 WHERE id=1", [])
            .unwrap();
        let compacted = read_at(&source, Limits::default()).unwrap();
        assert!(!compacted.bytes.starts_with(&before.bytes));
        assert!(
            String::from_utf8(compacted.bytes)
                .unwrap()
                .contains("Original marker")
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
        assert!(
            read_at(
                &source,
                Limits {
                    bytes: 8,
                    ..Limits::default()
                }
            )
            .is_err()
        );
        assert_eq!(
            conn.query_row("SELECT count(*) FROM messages", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn malformed_or_mixed_native_identity_is_not_accepted() {
        let raw = transcript();
        assert!(
            Hermes
                .parse(&raw.replace("\"session_id\":\"session-1\"", "\"session_id\":\"foreign\""))
                .is_err()
        );
        assert!(Hermes.parse("{}").is_err());
        assert!(
            Hermes
                .localize(&raw, "../escape", Path::new("/new"))
                .is_err()
        );
        assert!(
            Hermes
                .resume_command("session-1", Path::new("/new"), None, Some("unsupported"))
                .is_none()
        );
    }
}
