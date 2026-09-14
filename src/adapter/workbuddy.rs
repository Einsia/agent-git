//! WorkBuddy's native message stream keeps text and tool records as separate occurrences.

use super::native_message::{self as message, Call, Output};
use super::{
    Adapter, Capability, Event, EventKind, Installed, Next, OpenCall, Session, SessionRef,
    ToolDetails,
};
use crate::Result;
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct WorkBuddy;

pub fn home() -> Result<PathBuf> {
    for name in ["WORKBUDDY_CONFIG_DIR", "CODEBUDDY_CONFIG_DIR"] {
        if let Some(root) = std::env::var_os(name).filter(|root| !root.is_empty()) {
            return Ok(PathBuf::from(root));
        }
    }
    Ok(crate::infra::config::user_home()
        .context("the user home is not set")?
        .join(".workbuddy-ai"))
}

pub fn slug_for(cwd: &Path) -> String {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    cwd.to_string_lossy()
        .split(['/', '\\', ':', '-'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 256
            && id.as_bytes()[0].is_ascii_alphanumeric()
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-:".contains(&byte)),
        "invalid WorkBuddy session identity"
    );
    Ok(())
}

fn launch() -> Option<String> {
    if super::which("workbuddy-cli").is_some() {
        return Some("workbuddy-cli".into());
    }
    let bundled = Path::new(
        "/Applications/WorkBuddy AI.app/Contents/Resources/app.asar.unpacked/cli/dist/codebuddy.js",
    );
    let command = if bundled.is_file() && super::which("node").is_some() {
        format!("node {}", super::shell_arg(&bundled.to_string_lossy()))
    } else if super::which("codebuddy").is_some() {
        "codebuddy".into()
    } else {
        return None;
    };
    Some(format!(
        "CODEBUDDY_CONFIG_DIR={} {command}",
        super::shell_arg(&home().ok()?.to_string_lossy())
    ))
}

struct Parsed {
    session: Session,
    calls: Vec<Call>,
    outputs: Vec<Output>,
}

fn parse(text: &str) -> Result<Parsed> {
    let mut parsed = Parsed {
        session: Session {
            id: String::new(),
            runtime: "workbuddy".into(),
            cwd: None,
            events: vec![],
        },
        calls: vec![],
        outputs: vec![],
    };
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
        if let Some((id, cwd)) = WorkBuddy.record_identity(&value) {
            ensure!(
                parsed.session.id.is_empty() || parsed.session.id == id,
                "WorkBuddy transcript mixes session identities"
            );
            parsed.session.id = id;
            if let Some(cwd) = cwd {
                ensure!(
                    parsed
                        .session
                        .cwd
                        .as_ref()
                        .is_none_or(|current| current == &cwd),
                    "WorkBuddy transcript mixes working directories"
                );
                parsed.session.cwd = Some(cwd);
            }
        }
        let ts = message::timestamp(&value["timestamp"]);
        let mut events = Vec::new();
        match value["type"].as_str() {
            Some("message") => {
                let kind = match value["role"].as_str() {
                    Some("user") if value["isCompactSummary"] == true => EventKind::CompactSummary,
                    Some("user") => EventKind::UserPrompt,
                    Some("assistant") => EventKind::AssistantReply,
                    _ => EventKind::Other,
                };
                let text = message::text(&value["content"]);
                if !text.is_empty() {
                    events.push(Event::text(kind, text, ts.clone()));
                }
                if value["role"] == "assistant"
                    && value["status"] == "completed"
                    && (value["providerData"]["rawUsage"].is_object()
                        || value["message"]["usage"].is_object())
                {
                    events.push(Event::text(EventKind::TurnEnd, "", ts.clone()));
                }
                if let Some(blocks) = value["content"].as_array() {
                    for block in blocks.iter().filter(|block| {
                        !matches!(
                            block["type"].as_str(),
                            Some("input_text" | "output_text" | "text")
                        )
                    }) {
                        events.push(Event::text(
                            EventKind::Other,
                            block["text"].as_str().unwrap_or(""),
                            ts.clone(),
                        ));
                    }
                }
            }
            Some("function_call") => {
                let call = Call {
                    line,
                    id: value["callId"]
                        .as_str()
                        .context("WorkBuddy call has no callId")?
                        .into(),
                    name: value["name"]
                        .as_str()
                        .context("WorkBuddy call has no name")?
                        .into(),
                    input: message::input(&value["arguments"]),
                };
                events.push(message::tool_event(&call, ts.clone()));
                parsed.calls.push(call);
            }
            Some("function_call_result") => {
                let output = Output {
                    id: value["callId"]
                        .as_str()
                        .context("WorkBuddy result has no callId")?
                        .into(),
                    text: message::text(&value["output"]),
                    error: matches!(value["status"].as_str(), Some("failed" | "error"))
                        || value["isError"] == true,
                };
                events.push(Event::text(EventKind::ToolResult, &output.text, ts.clone()));
                parsed.outputs.push(output);
            }
            Some("system") if value["subtype"] == "compact_boundary" => {
                events.push(Event::text(EventKind::CompactSummary, "", ts))
            }
            _ => events.push(Event::text(EventKind::Other, "", ts)),
        }
        parsed
            .session
            .events
            .extend(events.into_iter().map(|event| event.at_line(line)));
    }
    Ok(parsed)
}

fn list_at(root: &Path, cwd: Option<&Path>) -> Result<Vec<SessionRef>> {
    let projects = root.join("projects");
    if !projects.exists() {
        return Ok(vec![]);
    }
    let start = cwd
        .map(|cwd| projects.join(slug_for(cwd)))
        .unwrap_or(projects);
    if !start.exists() {
        return Ok(vec![]);
    }
    let mut sessions = Vec::new();
    for entry in walkdir::WalkDir::new(&start)
        .min_depth(1)
        .max_depth(if cwd.is_some() { 1 } else { 2 })
    {
        let entry = entry?;
        if !entry.file_type().is_file() || entry.path().extension().is_none_or(|ext| ext != "jsonl")
        {
            continue;
        }
        let Some(id) = entry.path().file_stem().and_then(|id| id.to_str()) else {
            continue;
        };
        if validate_id(id).is_err() {
            continue;
        }
        sessions.push(SessionRef {
            id: id.into(),
            path: entry.path().into(),
            runtime: "workbuddy",
            // A lossy project slug cannot establish the transcript's recorded directory.
            cwd: None,
            mtime: entry.metadata()?.modified()?,
            gist: None,
        });
    }
    Ok(sessions)
}

fn install_at(root: &Path, content: &str, id: &str, cwd: &Path) -> Result<PathBuf> {
    validate_id(id)?;
    let content = WorkBuddy.localize(content, id, cwd)?;
    ensure!(
        WorkBuddy.parse(&content)?.id == id,
        "WorkBuddy history has no session identity"
    );
    let directory = root.join("projects").join(slug_for(cwd));
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{id}.jsonl"));
    let mut staged = tempfile::NamedTempFile::new_in(&directory)?;
    staged.write_all(content.as_bytes())?;
    staged.as_file().sync_all()?;
    staged
        .persist_noclobber(&path)
        .map_err(|error| error.error)?;
    Ok(path)
}

impl Adapter for WorkBuddy {
    fn id(&self) -> &'static str {
        "workbuddy"
    }
    fn cli(&self) -> &'static str {
        "workbuddy-cli"
    }
    fn format(&self) -> &'static str {
        "workbuddy"
    }
    fn capability(&self) -> Capability {
        Capability::Resumable
    }
    fn available(&self) -> bool {
        launch().is_some()
    }
    fn start_command(&self, _cwd: &Path) -> Option<String> {
        launch()
    }
    fn resume_command(
        &self,
        id: &str,
        _cwd: &Path,
        prompt: Option<&str>,
        system: Option<&str>,
    ) -> Option<String> {
        validate_id(id).ok()?;
        let mut command = format!("{} --resume {}", launch()?, super::shell_arg(id));
        if let Some(system) = system {
            command.push_str(&format!(
                " --append-system-prompt {}",
                super::shell_arg(system)
            ));
        }
        if let Some(prompt) = prompt {
            command.push_str(&format!(" {}", super::shell_arg(prompt)));
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
            .find(|event| event.kind != EventKind::Other)
            .is_some_and(|event| event.kind == EventKind::TurnEnd)
    }
    fn native_files_root(&self) -> Result<PathBuf> {
        Ok(home()?.join("projects"))
    }
    fn record_group(&self, value: &Value) -> Option<String> {
        self.record_identity(value)
            .map(|(id, _)| id)
            .filter(|id| !id.is_empty())
    }
    fn record_identity(&self, value: &Value) -> Option<(String, Option<String>)> {
        Some((
            value["sessionId"].as_str()?.into(),
            value["cwd"].as_str().map(str::to_owned),
        ))
    }
    fn sessions_for(&self, repo: &Path) -> Result<Vec<SessionRef>> {
        list_at(&home()?, Some(repo))
    }
    fn all_sessions(&self) -> Result<Vec<SessionRef>> {
        list_at(&home()?, None)
    }
    fn resolve(&self, id: &str, _cwd: Option<&Path>) -> Option<PathBuf> {
        validate_id(id).ok()?;
        self.lookup_native_readonly(id, Default::default())
            .ok()
            .map(|source| source.path)
    }
    fn parse(&self, text: &str) -> Result<Session> {
        Ok(parse(text)?.session)
    }
    fn line_details(&self, value: &Value) -> super::detail::LineDetails {
        use super::detail::{EventDetail, LineDetails, plaintext, summary};
        let mut details = LineDetails::default();
        match value["type"].as_str() {
            Some("function_call") => details.tools.push(EventDetail {
                input: Some(message::input(&value["arguments"])),
                ..Default::default()
            }),
            Some("message") => {
                if value["isCompactSummary"] == true {
                    details.compact = Some(summary(&message::text(&value["content"])));
                }
                if !matches!(value["role"].as_str(), Some("user" | "assistant"))
                    && !message::text(&value["content"]).is_empty()
                {
                    details.opaque.push(EventDetail {
                        reason: Some("injected_by_runtime"),
                        ..plaintext(&message::text(&value["content"]))
                    });
                }
                if let Some(blocks) = value["content"].as_array() {
                    for block in blocks.iter().filter(|block| {
                        !matches!(
                            block["type"].as_str(),
                            Some("input_text" | "output_text" | "text")
                        )
                    }) {
                        details.opaque.push(plaintext(
                            block["thinking"]
                                .as_str()
                                .or_else(|| block["text"].as_str())
                                .unwrap_or_default(),
                        ));
                    }
                }
            }
            Some("reasoning") => details
                .opaque
                .push(plaintext(&message::text(&value["content"]))),
            Some("system") if value["subtype"] == "compact_boundary" => {
                details.compact = Some(summary(""))
            }
            _ => {}
        }
        details
    }

    fn tool_details(&self, raw: &str, session: &Session, include_output: bool) -> ToolDetails {
        parse(raw)
            .map(|parsed| message::details(session, &parsed.calls, &parsed.outputs, include_output))
            .unwrap_or_default()
    }
    fn open_tool_calls(&self, raw: &str) -> Vec<OpenCall> {
        parse(raw)
            .map(|parsed| message::open_calls(&parsed.calls, &parsed.outputs, "function_call"))
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
        validate_id(id)?;
        let mut out = String::new();
        let mut parent = Value::Null;
        for (index, event) in session.events.iter().enumerate() {
            let key = message::remint(id, &index.to_string());
            let mut value = json!({"id":key,"parentId":parent,"sessionId":id,"cwd":cwd.to_string_lossy(),"timestamp":message::millis(event.timestamp.as_deref())});
            match event.kind {
                EventKind::UserPrompt
                | EventKind::UserInterjection
                | EventKind::AssistantReply
                | EventKind::CompactSummary
                | EventKind::CompactFiltered => {
                    let assistant = event.kind == EventKind::AssistantReply;
                    value["type"] = json!("message");
                    value["role"] = json!(if assistant { "assistant" } else { "user" });
                    value["content"] = json!([{"type":if assistant {"output_text"} else {"input_text"},"text":event.text.as_deref().unwrap_or("")}]);
                    if event.kind.is_compact() {
                        value["isCompactSummary"] = json!(true);
                    }
                    if assistant {
                        value["status"] = json!("completed");
                        value["message"] = json!({"usage":{}});
                    }
                }
                EventKind::ToolUse | EventKind::FileEdit if !details.is_receipt(index) => {
                    let detail = details.get(index).cloned().unwrap_or_default();
                    value["type"] = json!("function_call");
                    value["callId"] = json!(&key);
                    value["name"] = json!(event.tool.as_deref().unwrap_or("unknown"));
                    value["arguments"] = json!(detail.input.unwrap_or(json!({})).to_string());
                    out.push_str(&format!("{value}\n"));
                    value["id"] = json!(message::remint(id, &format!("result:{index}")));
                    value["parentId"] = json!(&key);
                    value["type"] = json!("function_call_result");
                    value["status"] = json!(if detail.error { "failed" } else { "completed" });
                    value["output"] = json!({"type":"text","text":detail.output.as_deref().unwrap_or(crate::domain::install::OPEN_CALL_PLACEHOLDER_OUTPUT)});
                    value
                        .as_object_mut()
                        .expect("record object")
                        .remove("arguments");
                }
                _ => continue,
            }
            parent = value["id"].clone();
            out.push_str(&format!("{value}\n"));
        }
        Ok(out)
    }
    fn localize(&self, content: &str, id: &str, cwd: &Path) -> Result<String> {
        // Native continuation records use the physical directory, so restored prefixes must too.
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        validate_id(id)?;
        let mut out = String::new();
        for raw in content.lines().filter(|line| !line.trim().is_empty()) {
            let mut value: Value = serde_json::from_str(raw)?;
            ensure!(value.is_object(), "WorkBuddy record is not an object");
            for field in ["id", "parentId", "callId"] {
                if let Some(old) = value[field].as_str() {
                    value[field] = json!(message::remint(id, old));
                }
            }
            if value.get("sessionId").is_some() {
                value["sessionId"] = json!(id);
            }
            if value.get("cwd").is_some() {
                value["cwd"] = json!(cwd.to_string_lossy());
            }
            out.push_str(&format!("{value}\n"));
        }
        for call in self.open_tool_calls(&out) {
            out.push_str(&format!("{}\n", json!({"type":"function_call_result","id":message::remint(id,&format!("missing:{}",call.call_id)),"sessionId":id,"cwd":cwd.to_string_lossy(),"callId":call.call_id,"name":call.name,"status":"failed","output":{"type":"text","text":crate::domain::install::OPEN_CALL_PLACEHOLDER_OUTPUT}})));
        }
        Ok(out)
    }
    fn install(&self, content: &str, id: &str, cwd: &Path) -> Result<Installed> {
        let command = self
            .resume_command(id, cwd, None, None)
            .context("install WorkBuddy or its CLI before restoring a session")?;
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

    const RAW: &str = concat!(
        "{\"type\":\"message\",\"id\":\"u\",\"sessionId\":\"session-1\",\"cwd\":\"/workspace\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"Read probe.txt\"}]}\n",
        "{\"type\":\"message\",\"id\":\"a\",\"parentId\":\"u\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"Reading it\"}]}\n",
        "{\"type\":\"function_call\",\"id\":\"a\",\"parentId\":\"u\",\"callId\":\"call-1\",\"name\":\"Read\",\"arguments\":\"{\\\"file_path\\\":\\\"probe.txt\\\"}\"}\n",
        "{\"type\":\"function_call_result\",\"id\":\"r\",\"parentId\":\"a\",\"callId\":\"call-1\",\"status\":\"completed\",\"output\":{\"type\":\"text\",\"text\":\"MARKER\"}}\n",
        "{\"type\":\"message\",\"id\":\"z\",\"parentId\":\"r\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"MARKER\"}],\"message\":{\"usage\":{}}}\n"
    );

    #[cfg(unix)]
    #[test]
    fn restoration_through_a_directory_alias_accepts_native_continuation() {
        let root = tempfile::tempdir().unwrap();
        let physical = root.path().join("physical");
        std::fs::create_dir(&physical).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&physical, &alias).unwrap();
        let mut raw = WorkBuddy.localize(RAW, "restored", &alias).unwrap();
        raw.push_str(&format!("{}\n", json!({"type":"message","id":"continued","sessionId":"restored","cwd":physical.canonicalize().unwrap(),"role":"user","content":"Continue"})));
        let session = WorkBuddy.parse(&raw).unwrap();
        assert_eq!(
            session.cwd.as_deref(),
            physical.canonicalize().unwrap().to_str()
        );
        assert_eq!(
            session.events.last().unwrap().text.as_deref(),
            Some("Continue")
        );
    }

    #[test]
    fn shared_message_ids_keep_both_text_and_tool_occurrences() {
        let session = WorkBuddy.parse(RAW).unwrap();
        assert_eq!(
            session
                .events
                .iter()
                .filter(|event| event.kind == EventKind::AssistantReply)
                .count(),
            2
        );
        let index = session
            .events
            .iter()
            .position(|event| event.kind == EventKind::ToolUse)
            .unwrap();
        let details = super::super::enrich::tool_details("workbuddy", RAW, &session);
        assert_eq!(
            details.get(index).unwrap().input.as_ref().unwrap()["file_path"],
            "probe.txt"
        );
        assert_eq!(
            details.get(index).unwrap().output.as_deref(),
            Some("MARKER")
        );
        assert!(WorkBuddy.open_tool_calls(RAW).is_empty());
        assert_eq!(crate::domain::turn::completed_count(&session), 1);
        let incomplete = RAW.lines().take(3).collect::<Vec<_>>().join("\n") + "\n";
        assert_eq!(WorkBuddy.open_tool_calls(&incomplete).len(), 1);
        assert_eq!(
            crate::domain::turn::completed_count(&WorkBuddy.parse(&incomplete).unwrap()),
            0
        );
    }

    #[test]
    fn localization_preserves_vendor_fields_and_tool_pairing() {
        let raw = RAW.replace(
            "\"status\":\"completed\"",
            "\"status\":\"completed\",\"vendor\":{\"opaque\":true}",
        );
        let localized = WorkBuddy
            .localize(&raw, "new-id", Path::new("/new workspace"))
            .unwrap();
        let session = WorkBuddy.parse(&localized).unwrap();
        assert_eq!(session.id, "new-id");
        assert_eq!(session.cwd.as_deref(), Some("/new workspace"));
        assert!(localized.contains("\"opaque\":true"));
        assert!(WorkBuddy.open_tool_calls(&localized).is_empty());
        let rows: Vec<Value> = localized
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows[1]["id"], rows[2]["id"]);
        assert_eq!(rows[2]["callId"], rows[3]["callId"]);
        let tail = RAW.lines().take(3).collect::<Vec<_>>().join("\n");
        assert!(
            WorkBuddy
                .open_tool_calls(
                    &WorkBuddy
                        .localize(&tail, "new-id", Path::new("/new"))
                        .unwrap()
                )
                .is_empty()
        );
    }

    #[test]
    fn candidate_enumeration_defers_transcript_fields_and_reads() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let directory = root.path().join("projects").join(slug_for(cwd.path()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("filename-hint.jsonl");
        std::fs::write(&path, RAW).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o0)).unwrap();
        }
        let scoped = list_at(root.path(), Some(cwd.path()));
        let all = list_at(root.path(), None);
        #[cfg(unix)]
        std::fs::set_permissions(&path, metadata.permissions()).unwrap();
        for sessions in [scoped.unwrap(), all.unwrap()] {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].id, "filename-hint");
            assert_eq!(sessions[0].path, path);
            assert_eq!(sessions[0].runtime, "workbuddy");
            assert_eq!(sessions[0].mtime, metadata.modified().unwrap());
            assert!(sessions[0].cwd.is_none());
            assert!(sessions[0].gist.is_none());
        }
        let selected = WorkBuddy.parse_at(&path).unwrap();
        assert_eq!(selected.id, "session-1");
        assert_eq!(selected.cwd.as_deref(), Some("/workspace"));
    }

    #[test]
    fn colliding_project_slugs_do_not_establish_recorded_directories() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("a-b");
        let other = root.path().join("a").join("b");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        assert_eq!(slug_for(&cwd), slug_for(&other));
        let path = install_at(root.path(), RAW, "shared-slug", &cwd).unwrap();
        let directory = path.parent().unwrap();
        let subagents = directory.join("shared-slug").join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(subagents.join("agent-child.jsonl"), RAW).unwrap();
        std::fs::write(directory.join("invalid.id.jsonl"), RAW).unwrap();
        std::fs::create_dir(directory.join("directory.jsonl")).unwrap();
        for wanted in [Some(cwd.as_path()), Some(other.as_path()), None] {
            let sessions = list_at(root.path(), wanted).unwrap();
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].path, path);
            assert!(sessions[0].cwd.is_none());
        }
        assert_eq!(
            WorkBuddy.parse_at(&path).unwrap().cwd.as_deref(),
            cwd.canonicalize().unwrap().to_str()
        );
    }

    #[test]
    fn installation_is_discoverable_and_never_overwrites_a_native_session() {
        let root = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let path = install_at(root.path(), RAW, "restored", cwd.path()).unwrap();
        let sessions = list_at(root.path(), Some(cwd.path())).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "restored");
        assert_eq!(sessions[0].path, path);
        let before = std::fs::read(&path).unwrap();
        assert!(install_at(root.path(), "", "restored", cwd.path()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert!(install_at(root.path(), RAW, "../escape", cwd.path()).is_err());
    }
}
