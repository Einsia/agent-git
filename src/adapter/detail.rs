//! Native presentation details remain separate from the lossy session IR.
//! Source coordinates and category ordinals bind each detail to its parsed event.
//! Consumers use `Adapter::event_details` so native branch selection is respected.

use super::{Adapter, EventKind, Session};
use serde::Serialize;

pub const TEXT_LIMIT: usize = 8000;

#[derive(Debug, Clone, Serialize, Default)]
pub struct EventDetail {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_len: Option<usize>,
}

#[derive(Debug, Default)]
pub struct LineDetails {
    pub(crate) tools: Vec<EventDetail>,
    pub(crate) opaque: Vec<EventDetail>,
    pub(crate) compact: Option<EventDetail>,
}

pub fn of_line(runtime: &str, line: &str) -> LineDetails {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
        return LineDetails::default();
    };
    super::get(runtime)
        .map(|adapter| adapter.line_details(&v))
        .unwrap_or_default()
}

pub(crate) fn legacy_line(runtime: &str, v: &serde_json::Value) -> LineDetails {
    match runtime {
        "claude-code" | "cursor" => claude_code_line(v),
        "codex" => codex_line(v),
        "opencode" => opencode_line(v),
        _ => LineDetails::default(),
    }
}

/// Pair details by source line and category; interleaved text never shifts tool ordinals.
pub(crate) fn project<A: Adapter + ?Sized>(
    adapter: &A,
    raw: &str,
    session: &Session,
    selected: impl Fn(usize) -> bool,
) -> Vec<Option<EventDetail>> {
    let lines: Vec<_> = raw.lines().collect();
    let mut cache = std::collections::HashMap::new();
    let mut seen = std::collections::HashMap::new();
    session
        .events
        .iter()
        .map(|event| {
            let line = event.line?;
            if !selected(line) {
                return None;
            }
            let bucket = match event.kind {
                EventKind::ToolUse | EventKind::FileEdit => "tool",
                EventKind::Other => "opaque",
                _ => "text",
            };
            let nth = seen.entry((line, bucket)).or_insert(0);
            let details = cache.entry(line).or_insert_with(|| {
                lines
                    .get(line)
                    .and_then(|line| serde_json::from_str(line).ok())
                    .map(|value| adapter.line_details(&value))
                    .unwrap_or_default()
            });
            let result = details.take(event.kind, *nth);
            *nth += 1;
            result
        })
        .collect()
}

pub(crate) fn plaintext(body: &str) -> EventDetail {
    let (text, truncated, full_len) = clip(body);
    EventDetail {
        text: (!text.is_empty()).then_some(text),
        truncated,
        full_len: Some(full_len),
        ..Default::default()
    }
}

pub(crate) fn summary(body: &str) -> EventDetail {
    EventDetail {
        compact: Some(serde_json::json!({"lossy":true})),
        ..plaintext(body)
    }
}

impl LineDetails {
    pub fn take(&self, kind: EventKind, nth: usize) -> Option<EventDetail> {
        match kind {
            EventKind::ToolUse | EventKind::FileEdit => self.tools.get(nth).cloned(),
            EventKind::Other => self.opaque.get(nth).cloned(),
            EventKind::CompactFiltered | EventKind::CompactSummary => self.compact.clone(),
            EventKind::UserPrompt
            | EventKind::UserInterjection
            | EventKind::AssistantReply
            | EventKind::ToolResult
            | EventKind::TurnEnd => None,
        }
    }
}

fn claude_code_line(v: &serde_json::Value) -> LineDetails {
    let mut d = LineDetails::default();
    let content = v.get("message").and_then(|m| m.get("content"));

    if v.get("isCompactSummary").and_then(|x| x.as_bool()) == Some(true) {
        let body = content.and_then(|c| c.as_str()).unwrap_or_default();
        let (text, truncated, full) = clip(body);
        d.compact = Some(EventDetail {
            text: (!text.is_empty()).then_some(text),
            compact: Some(serde_json::json!({ "summary_len": full, "lossy": true })),
            truncated,
            full_len: Some(full),
            ..Default::default()
        });
        return d;
    }

    for b in content
        .and_then(|c| c.as_array())
        .map(|a| a.as_slice())
        .unwrap_or_default()
    {
        match b.get("type").and_then(|x| x.as_str()).unwrap_or("") {
            "tool_use" => d.tools.push(EventDetail {
                input: b.get("input").cloned(),
                ..Default::default()
            }),
            "thinking" => {
                let body = b
                    .get("thinking")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default();
                let (text, truncated, full) = clip(body);
                let empty = text.is_empty();
                d.opaque.push(EventDetail {
                    text: (!empty).then_some(text),
                    reason: empty.then_some("empty_thinking"),
                    truncated,
                    full_len: Some(full),
                    ..Default::default()
                });
            }
            "redacted_thinking" => d.opaque.push(EventDetail {
                reason: Some("redacted_thinking"),
                ..Default::default()
            }),
            _ => {}
        }
    }

    if d.opaque.is_empty()
        && v.get("type").and_then(|x| x.as_str()) == Some("user")
        && let Some(body) = content.and_then(|c| c.as_str()).or_else(|| {
            content
                .and_then(|c| c.as_array())
                .and_then(|a| a.first())
                .and_then(|b| b.get("text"))
                .and_then(|x| x.as_str())
        })
    {
        let (text, truncated, full) = clip(body);
        d.opaque.push(EventDetail {
            text: Some(text),
            reason: Some("injected_by_runtime"),
            truncated,
            full_len: Some(full),
            ..Default::default()
        });
    }

    d
}

fn codex_line(v: &serde_json::Value) -> LineDetails {
    let mut d = LineDetails::default();
    let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
    let payload = v.get("payload");

    match ty {
        "compacted" => {
            let p = payload;
            let kept = p
                .and_then(|p| p.get("replacement_history"))
                .and_then(|x| x.as_array())
                .map(|a| a.len());
            d.compact = Some(EventDetail {
                compact: Some(serde_json::json!({
                    "lossy": false,
                    "window_number": p.and_then(|p| p.get("window_number")).cloned(),
                    "kept": kept,
                    "previous_window_id": p.and_then(|p| p.get("previous_window_id")).cloned(),
                    "first_window_id": p.and_then(|p| p.get("first_window_id")).cloned(),
                })),
                ..Default::default()
            });
        }
        "response_item" => {
            let Some(p) = payload else { return d };
            match p.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                "custom_tool_call" => {
                    d.tools.push(EventDetail {
                        input: p.get("input").cloned(),
                        ..Default::default()
                    });
                }
                "function_call" | "local_shell_call" => {
                    let raw = p
                        .get("arguments")
                        .or_else(|| p.get("action"))
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    let input = match raw.as_str() {
                        Some(s) => serde_json::from_str(s).unwrap_or(raw.clone()),
                        None => raw,
                    };
                    d.tools.push(EventDetail {
                        input: (!input.is_null()).then_some(input),
                        ..Default::default()
                    });
                }
                "reasoning" => d.opaque.push(EventDetail {
                    reason: Some("encrypted_reasoning"),
                    ..Default::default()
                }),
                "message" => {
                    if p.get("role").and_then(|x| x.as_str()) == Some("user")
                        && let Some(body) = codex_text(p.get("content"))
                    {
                        let (text, truncated, full) = clip(&body);
                        d.opaque.push(EventDetail {
                            text: Some(text),
                            reason: Some("injected_by_runtime"),
                            truncated,
                            full_len: Some(full),
                            ..Default::default()
                        });
                    }
                }
                _ => {}
            }
        }
        "event_msg" => {
            let Some(p) = payload else { return d };
            if p.get("type").and_then(|x| x.as_str()) == Some("patch_apply_end") {
                d.tools.push(EventDetail {
                    changes: p.get("changes").cloned(),
                    ..Default::default()
                });
            }
        }
        _ => {}
    }
    d
}

fn codex_text(content: Option<&serde_json::Value>) -> Option<String> {
    let arr = content?.as_array()?;
    let parts: Vec<&str> = arr
        .iter()
        .filter_map(|b| b.get("text").and_then(|x| x.as_str()))
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn opencode_line(v: &serde_json::Value) -> LineDetails {
    let mut details = LineDetails::default();
    if v.get("kind").and_then(|value| value.as_str()) != Some("part") {
        return details;
    }
    let Some(data) = v.get("data") else {
        return details;
    };
    match data.get("type").and_then(|value| value.as_str()) {
        Some("reasoning") => {
            let body = data.get("text").and_then(|value| value.as_str());
            let (text, truncated, full_len) = clip(body.unwrap_or_default());
            details.opaque.push(EventDetail {
                reason: text.is_empty().then_some("empty_reasoning"),
                text: (!text.is_empty()).then_some(text),
                truncated,
                full_len: Some(full_len),
                ..Default::default()
            });
        }
        Some("compaction") => {
            let mut parameters = serde_json::Map::new();
            parameters.insert("lossy".into(), true.into());
            for key in ["auto", "overflow"] {
                if let Some(value) = data.get(key).and_then(|value| value.as_bool()) {
                    parameters.insert(key.into(), value.into());
                }
            }
            if let Some(value) = data.get("tail_start_id").and_then(|value| value.as_str()) {
                parameters.insert("tail_start_id".into(), value.into());
            }
            details.compact = Some(EventDetail {
                compact: Some(parameters.into()),
                ..Default::default()
            });
        }
        _ => {}
    }
    details
}

fn clip(s: &str) -> (String, bool, usize) {
    let n = s.chars().count();
    if n <= TEXT_LIMIT {
        return (s.to_string(), false, n);
    }
    (s.chars().take(TEXT_LIMIT).collect(), true, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_arguments_follow_parsed_event_coordinates() {
        let cases = [
            (
                "workbuddy",
                concat!(
                    "{\"type\":\"message\",\"sessionId\":\"s\",\"role\":\"user\",\"content\":\"Read\"}\n",
                    "{\"type\":\"function_call\",\"callId\":\"c\",\"name\":\"Read\",\"arguments\":\"{\\\"path\\\":\\\"probe.txt\\\"}\"}\n"
                ),
            ),
            (
                "hermes",
                concat!(
                    "{\"type\":\"hermes_session\",\"version\":1,\"data\":{\"id\":\"s\"}}\n",
                    "{\"type\":\"hermes_message\",\"data\":{\"session_id\":\"s\",\"role\":\"assistant\",\"tool_calls\":[{\"id\":\"c\",\"function\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"probe.txt\"}}}],\"reasoning_content\":\"Inspect file\"}}\n"
                ),
            ),
        ];
        for (runtime, raw) in cases {
            let adapter = crate::adapter::get(runtime).unwrap();
            let session = adapter.parse(raw).unwrap();
            let details = adapter.event_details(raw, &session);
            let index = session
                .events
                .iter()
                .position(|event| matches!(event.kind, EventKind::ToolUse | EventKind::FileEdit))
                .unwrap();
            assert_eq!(
                details[index].as_ref().unwrap().input.as_ref().unwrap()["path"],
                "probe.txt"
            );
            if runtime == "hermes" {
                let index = session
                    .events
                    .iter()
                    .position(|event| event.kind == EventKind::Other)
                    .unwrap();
                assert_eq!(
                    details[index].as_ref().unwrap().text.as_deref(),
                    Some("Inspect file")
                );
            }
        }
    }

    #[test]
    fn inactive_hermes_records_do_not_disclose_reasoning_as_active_details() {
        let line = serde_json::json!({"type":"hermes_message", "data":{"active":0,"reasoning_content":"Inactive reasoning"}});
        assert!(
            of_line("hermes", &line.to_string())
                .take(EventKind::Other, 0)
                .is_none()
        );
    }

    #[test]
    fn claude_code_blocks_land_in_the_right_buckets() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[
            {"type":"thinking","thinking":"Inspect EXIF first"},
            {"type":"text","text":"Okay"},
            {"type":"tool_use","name":"Bash","input":{"command":"grep -rn foo src/"}}]}}"#;
        let d = of_line("claude-code", line);

        let tool = d.take(EventKind::ToolUse, 0).unwrap();
        assert_eq!(
            tool.input.unwrap()["command"],
            "grep -rn foo src/",
            "Tool arguments must remain available"
        );
        let think = d.take(EventKind::Other, 0).unwrap();
        assert_eq!(think.text.as_deref(), Some("Inspect EXIF first"));
        assert!(
            think.reason.is_none(),
            "Plaintext reasoning needs no missing-body reason"
        );
        assert!(d.take(EventKind::AssistantReply, 0).is_none());
    }

    #[test]
    fn multiple_tool_calls_on_one_line_stay_in_order() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"Read","input":{"file_path":"/a.rs"}},
            {"type":"text","text":"Interleaved text"},
            {"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#;
        let d = of_line("claude-code", line);
        assert_eq!(
            d.take(EventKind::FileEdit, 0).unwrap().input.unwrap()["file_path"],
            "/a.rs"
        );
        assert_eq!(
            d.take(EventKind::ToolUse, 1).unwrap().input.unwrap()["command"],
            "cargo test",
            "Text blocks must not shift tool ordinals"
        );
        assert!(d.take(EventKind::ToolUse, 2).is_none());
    }

    #[test]
    fn opaque_events_say_why_they_have_no_body() {
        let redacted = of_line(
            "claude-code",
            r#"{"type":"assistant","message":{"content":[{"type":"redacted_thinking","data":"…"}]}}"#,
        );
        let r = redacted.take(EventKind::Other, 0).unwrap();
        assert!(r.text.is_none());
        assert_eq!(r.reason, Some("redacted_thinking"));

        let codex = of_line(
            "codex",
            r#"{"type":"response_item","payload":{"type":"reasoning","encrypted_content":"…"}}"#,
        );
        let c = codex.take(EventKind::Other, 0).unwrap();
        assert_eq!(
            c.reason,
            Some("encrypted_reasoning"),
            "Encrypted reasoning must remain distinct from plaintext"
        );
    }

    #[test]
    fn codex_arguments_are_unwrapped_from_their_json_string() {
        let line = r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"bash\",\"-lc\",\"ls\"]}"}}"#;
        let d = of_line("codex", line);
        let input = d.take(EventKind::ToolUse, 0).unwrap().input.unwrap();
        assert_eq!(
            input["command"][2], "ls",
            "Decode serialized JSON arguments"
        );
    }

    #[test]
    fn the_two_compact_mechanisms_are_reported_differently() {
        let filtered = of_line(
            "codex",
            r#"{"type":"compacted","payload":{"window_number":3,"replacement_history":[1,2,3],"previous_window_id":"w2"}}"#,
        );
        let f = filtered.take(EventKind::CompactFiltered, 0).unwrap();
        let c = f.compact.unwrap();
        assert_eq!(c["lossy"], false);
        assert_eq!(c["window_number"], 3);
        assert_eq!(c["kept"], 3);
        assert!(
            f.text.is_none(),
            "Filtered history must not duplicate message bodies"
        );

        let summary = of_line(
            "claude-code",
            r#"{"type":"user","isCompactSummary":true,"message":{"role":"user","content":"This session is being continued…"}}"#,
        );
        let s = summary.take(EventKind::CompactSummary, 0).unwrap();
        assert_eq!(s.compact.unwrap()["lossy"], true);
        assert!(s.text.is_some(), "Summary bodies must remain available");
    }

    #[test]
    fn runtime_injected_records_are_explained() {
        let d = of_line(
            "claude-code",
            r#"{"type":"user","message":{"role":"user","content":"<task-notification> done"}}"#,
        );
        let o = d.take(EventKind::Other, 0).unwrap();
        assert_eq!(o.reason, Some("injected_by_runtime"));
        assert!(o.text.unwrap().contains("task-notification"));
    }

    #[test]
    fn long_bodies_are_clipped_and_say_so() {
        // This fixture exercises CJK character clipping.
        let long = "字".repeat(TEXT_LIMIT + 500);
        let line = serde_json::json!({
            "type": "assistant",
            "message": { "content": [{ "type": "thinking", "thinking": long }] }
        })
        .to_string();
        let d = of_line("claude-code", &line);
        let t = d.take(EventKind::Other, 0).unwrap();
        assert!(t.truncated, "Truncation must be explicit");
        assert_eq!(
            t.full_len,
            Some(TEXT_LIMIT + 500),
            "Include original character length"
        );
        assert_eq!(
            t.text.unwrap().chars().count(),
            TEXT_LIMIT,
            "Clipping must preserve Unicode characters"
        );
    }

    #[test]
    fn a_broken_line_yields_nothing_instead_of_failing() {
        let d = of_line("claude-code", "NOT JSON {");
        assert!(d.take(EventKind::ToolUse, 0).is_none());
        assert!(d.take(EventKind::Other, 0).is_none());
    }

    #[test]
    fn opencode_reasoning_uses_the_native_line_and_character_limit() {
        let body = "🦀".repeat(TEXT_LIMIT + 1);
        let raw = format!(
            "{}\n{}\n",
            serde_json::json!({"kind":"message", "id":"assistant", "data":{"role":"assistant"}}),
            serde_json::json!({"kind":"part", "message_id":"assistant", "data":{"type":"reasoning", "text":body}}),
        );
        let parser = crate::adapter::get("opencode").unwrap();
        let session = parser.parse(&raw).unwrap();
        let event = session.events.first().unwrap();
        assert_eq!(event.kind, EventKind::Other);
        assert_eq!(event.line, Some(1));
        assert!(event.text.is_none());
        let line = raw.lines().nth(event.line.unwrap()).unwrap();
        let details = of_line(parser.format(), line);
        let detail = details.take(event.kind, 0).unwrap();
        assert!(detail.truncated);
        assert_eq!(detail.full_len, Some(TEXT_LIMIT + 1));
        assert_eq!(detail.text.unwrap(), "🦀".repeat(TEXT_LIMIT));
        assert!(detail.reason.is_none());
        assert!(details.take(event.kind, 1).is_none());
        let original: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(original["data"]["text"], body);
    }

    #[test]
    fn opencode_compaction_parameters_follow_the_correlated_boundary() {
        let records = [
            serde_json::json!({"kind":"message", "id":"boundary", "data":{"role":"user"}}),
            serde_json::json!({"kind":"part", "message_id":"boundary", "data":{"type":"compaction", "auto":false, "overflow":true, "tail_start_id":"tail", "unmodeled":"hidden"}}),
            serde_json::json!({"kind":"message", "id":"summary", "data":{"role":"assistant", "mode":"compaction"}}),
            serde_json::json!({"kind":"part", "message_id":"summary", "data":{"type":"text", "text":"Selected summary"}}),
        ];
        let raw = records
            .iter()
            .map(|record| format!("{record}\n"))
            .collect::<String>();
        let parser = crate::adapter::get("opencode").unwrap();
        let session = parser.parse(&raw).unwrap();
        let event = session.events.first().unwrap();
        assert_eq!(event.kind, EventKind::CompactSummary);
        assert_eq!(event.line, Some(1));
        assert_eq!(event.text.as_deref(), Some("Selected summary"));
        let line = raw.lines().nth(event.line.unwrap()).unwrap();
        let detail = of_line(parser.format(), line).take(event.kind, 0).unwrap();
        assert_eq!(
            detail.compact.unwrap(),
            serde_json::json!({"lossy":true, "auto":false, "overflow":true, "tail_start_id":"tail"}),
        );
        assert!(detail.text.is_none());
        assert!(detail.full_len.is_none());
        assert!(!detail.truncated);
    }

    #[test]
    fn opencode_details_do_not_guess_from_unrelated_shapes() {
        let claude = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"Claude reasoning"}]}}"#;
        for format in ["future-runtime", "opencode"] {
            assert!(of_line(format, claude).take(EventKind::Other, 0).is_none());
        }
        for record in [
            serde_json::json!({"kind":"message", "data":{"type":"reasoning", "text":"not a part"}}),
            serde_json::json!({"kind":"part", "data":{"type":"text", "text":"native message"}}),
        ] {
            assert!(
                of_line("opencode", &record.to_string())
                    .take(EventKind::Other, 0)
                    .is_none()
            );
        }
        let empty = of_line(
            "opencode",
            r#"{"kind":"part","data":{"type":"reasoning","text":""}}"#,
        )
        .take(EventKind::Other, 0)
        .unwrap();
        assert_eq!(empty.reason, Some("empty_reasoning"));
        assert_eq!(empty.full_len, Some(0));
        assert!(empty.text.is_none());
        let malformed_parameters = of_line(
            "opencode",
            r#"{"kind":"part","data":{"type":"compaction","auto":"true","overflow":1,"tail_start_id":{}}}"#,
        )
        .take(EventKind::CompactSummary, 0)
        .unwrap();
        assert_eq!(
            malformed_parameters.compact.unwrap(),
            serde_json::json!({"lossy":true}),
        );
    }
    #[test]
    fn codex_custom_tool_input_stays_native_without_output_pairing() {
        let raw = serde_json::json!({"type":"response_item", "payload":{"type":"custom_tool_call", "name":"exec_command", "call_id":"same-id", "input":"{native command text}"}}).to_string();
        let details = of_line("codex", &raw);
        assert_eq!(
            details.take(EventKind::ToolUse, 0).unwrap().input,
            Some(serde_json::json!("{native command text}"))
        );
    }
    #[test]
    fn cursor_native_tool_blocks_keep_their_input_details() {
        let raw = serde_json::json!({"type":"assistant", "message":{"role":"assistant", "content":[{"type":"tool_use","name":"Shell","input":{"command":"printf cursor-input"}}]}}).to_string();
        let parser = crate::adapter::get("cursor").unwrap();
        let session = parser.parse(&raw).unwrap();
        assert_eq!(session.events[0].kind, EventKind::ToolUse);
        assert_eq!(
            of_line(parser.format(), &raw)
                .take(EventKind::ToolUse, 0)
                .unwrap()
                .input,
            Some(serde_json::json!({"command":"printf cursor-input"}))
        );
    }
}
