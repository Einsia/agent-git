//! Go back to the source transcript by `Event.line` for a tool call's arguments and its paired
//! output.
//!
//! The IR deliberately carries none of this (the comment on `Event::line` is where that decision
//! is recorded): stuffing it in defeats "the IR is deliberately small", and every
//! runtime-proprietary field then means touching the IR. This is the implementation of what that
//! comment calls "a consumer takes the coordinates back to the raw jsonl and reads it there" —
//! the consumer is cross-runtime installation ([`crate::domain::install`]), and the details it
//! reads back reach the target runtime's `render_with` through [`ToolDetails`].
//!
//! # Pairing by id, locating by (line, index within the line)
//!
//! Output ownership recognizes **only** the source side's own pairing id (`tool_use.id` ↔
//! `tool_result.tool_use_id`, `call_id`); it does not guess from order of appearance — parallel
//! calls fix no relation between output order and call order. Which call in the source line the
//! i-th event in the IR corresponds to is located by (line number, which one within that line):
//! one line can produce several tool events (Claude's block array), and the comment on
//! `Event.line` establishes that a consumer pairs same-kind events within one line in order.
//!
//! # What cannot be read stays empty
//!
//! A source file that is not line-per-JSON (OpenCode's export form), a line number that does not
//! line up, a missing id — all yield an empty table or a missing entry, and the rendering side
//! falls back to placeholder output. Enrichment only makes the artifact more complete; it never
//! makes installation fail.

use super::{EventKind, Session, ToolDetail, ToolDetails};
use std::collections::HashMap;

/// Read back, from the source transcript, the details of every tool event in `session`.
///
/// `format` is the source runtime's format family ([`super::Adapter::format`]), and `raw` is the
/// same text that was fed to parse — line-number coordinates only mean anything against the same
/// text.
pub fn tool_details(format: &str, raw: &str, session: &Session) -> ToolDetails {
    let lines: Vec<&str> = raw.lines().collect();
    let mut out = ToolDetails::default();
    match format {
        "claude-code" => claude(&lines, session, &mut out),
        "codex" => codex(&lines, session, &mut out),
        "opencode" => opencode(&lines, session, &mut out),
        // Unknown format family: better an empty table than reading against a guessed shape.
        _ => {}
    }
    out
}

/// Apply a (line number, index within the line) lookup to every tool event in the session.
///
/// The index within the line counts ToolUse and FileEdit together: they come from one run of call
/// blocks on the same line, and counting them separately stops lining up with the order of the
/// blocks in the source line.
fn assign(
    session: &Session,
    out: &mut ToolDetails,
    f: impl Fn(usize, usize) -> Option<ToolDetail>,
) {
    let mut seen: HashMap<usize, usize> = HashMap::new();
    for (i, e) in session.events.iter().enumerate() {
        if !matches!(e.kind, EventKind::ToolUse | EventKind::FileEdit) {
            continue;
        }
        let Some(line) = e.line else { continue };
        let k = seen.entry(line).or_insert(0);
        if let Some(d) = f(line, *k) {
            out.insert(i, d);
        }
        *k += 1;
    }
}

fn parse_line(l: &str) -> Option<serde_json::Value> {
    serde_json::from_str(l.trim()).ok()
}

/// The three forms of `tool_result.content`: a string, an array of text blocks, anything else.
/// Any other form is serialized unchanged — that structure is what the model saw at the time.
///
/// An empty string still returns `Some("")`: "returned successfully with no output" and "the
/// output was not read back" are two different facts, and conflating them into `None` leaves the
/// former wearing a missing-output placeholder on the target side.
fn claude_result_text(content: Option<&serde_json::Value>) -> Option<String> {
    let c = content?;
    Some(match c {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    })
}

fn claude(lines: &[&str], session: &Session, out: &mut ToolDetails) {
    // One pass collects calls (by line, in block order), one collects outputs (by tool_use_id).
    let mut calls_at: HashMap<usize, Vec<(String, serde_json::Value)>> = HashMap::new();
    let mut outputs: HashMap<String, (String, bool)> = HashMap::new();
    for (ln, l) in lines.iter().enumerate() {
        let Some(v) = parse_line(l) else { continue };
        let Some(blocks) = v.pointer("/message/content").and_then(|c| c.as_array()) else {
            continue;
        };
        for b in blocks {
            match b.get("type").and_then(|x| x.as_str()) {
                Some("tool_use") => {
                    let id = b
                        .get("id")
                        .and_then(|x| x.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let input = b.get("input").cloned().unwrap_or(serde_json::json!({}));
                    calls_at.entry(ln).or_default().push((id, input));
                }
                Some("tool_result") => {
                    if let (Some(id), Some(text)) = (
                        b.get("tool_use_id").and_then(|x| x.as_str()),
                        claude_result_text(b.get("content")),
                    ) {
                        let err = b.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
                        outputs.insert(id.to_string(), (text, err));
                    }
                }
                _ => {}
            }
        }
    }
    assign(session, out, |line, k| {
        let (id, input) = calls_at.get(&line)?.get(k)?.clone();
        let (output, error) = match outputs.get(&id).cloned() {
            Some((t, e)) => (Some(t), e),
            None => (None, false),
        };
        Some(ToolDetail {
            input: Some(input),
            output,
            error,
        })
    });
}

fn codex(lines: &[&str], session: &Session, out: &mut ToolDetails) {
    // Codex records one call per line; the three families use different argument keys, while
    // outputs uniformly pair by call_id.
    let mut calls_at: HashMap<usize, (String, serde_json::Value)> = HashMap::new();
    let mut outputs: HashMap<String, String> = HashMap::new();
    for (ln, l) in lines.iter().enumerate() {
        let Some(v) = parse_line(l) else { continue };
        if v.get("type").and_then(|x| x.as_str()) != Some("response_item") {
            continue;
        }
        let Some(p) = v.get("payload") else { continue };
        let call_id = p
            .get("call_id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        match p.get("type").and_then(|x| x.as_str()) {
            Some("function_call") => {
                // arguments is a JSON-encoded string; when it decodes, hand over the structure;
                // when it does not, the raw text as a string — both are more honest than an
                // empty object.
                let input = p
                    .get("arguments")
                    .and_then(|x| x.as_str())
                    .map(|s| {
                        serde_json::from_str(s)
                            .unwrap_or_else(|_| serde_json::Value::String(s.to_string()))
                    })
                    .unwrap_or(serde_json::json!({}));
                calls_at.insert(ln, (call_id, input));
            }
            Some("custom_tool_call") => {
                let input = p.get("input").cloned().unwrap_or(serde_json::json!({}));
                calls_at.insert(ln, (call_id, input));
            }
            Some("local_shell_call") => {
                let input = p.get("action").cloned().unwrap_or(serde_json::json!({}));
                calls_at.insert(ln, (call_id, input));
            }
            Some(
                "function_call_output" | "custom_tool_call_output" | "local_shell_call_output",
            ) => {
                // output is either plain text or {"output": ..., "metadata": ...}.
                let text = match p.get("output") {
                    Some(serde_json::Value::String(s)) => Some(s.clone()),
                    Some(o) => o
                        .get("output")
                        .and_then(|x| x.as_str())
                        .map(String::from)
                        .or_else(|| Some(o.to_string())),
                    None => None,
                };
                // An empty string is still collected: same reason as in claude_result_text.
                if let Some(t) = text
                    && !call_id.is_empty()
                {
                    outputs.insert(call_id, t);
                }
            }
            _ => {}
        }
    }
    assign(session, out, |line, _k| {
        let (id, input) = calls_at.get(&line)?.clone();
        Some(ToolDetail {
            input: Some(input),
            output: outputs.get(&id).cloned(),
            error: false,
        })
    });
    // Every FileEdit in a Codex source is a projection of patch_apply_end: the real call is
    // another ToolUse event in the same transcript. Mark it a receipt and the rendering side no
    // longer mints a second call.
    for (i, e) in session.events.iter().enumerate() {
        if e.kind == EventKind::FileEdit {
            out.mark_receipt(i);
        }
    }
}

fn opencode(lines: &[&str], session: &Session, out: &mut ToolDetails) {
    // In the canonical line set a tool part is one per line and its input and output sit in the
    // state on that same line, so nothing pairs across lines. The export form is not
    // line-per-JSON, so reaching here yields an empty table.
    assign(session, out, |line, _k| {
        let v = parse_line(lines.get(line)?)?;
        let state = v.pointer("/data/state")?;
        let input = state.get("input").cloned();
        let error = state.get("status").and_then(|x| x.as_str()) == Some("error");
        // A failed call's body is in `state.error`; an empty-string output is still carried
        // faithfully (see claude_result_text).
        let output = if error {
            state
                .get("error")
                .and_then(|x| x.as_str())
                .map(String::from)
                .or_else(|| {
                    state
                        .get("output")
                        .and_then(|x| x.as_str())
                        .map(String::from)
                })
        } else {
            state
                .get("output")
                .and_then(|x| x.as_str())
                .map(String::from)
        };
        if input.is_none() && output.is_none() {
            return None;
        }
        Some(ToolDetail {
            input,
            output,
            error,
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::Adapter as _;

    /// Claude source: two tool_use blocks on one line pair to their own call by id even when the
    /// outputs come back out of order.
    #[test]
    fn claude_details_pair_by_id_not_by_order() {
        let raw = concat!(
            r#"{"type":"user","sessionId":"s","uuid":"u1","message":{"role":"user","content":"do it"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s","uuid":"u2","parentUuid":"u1","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -l"}},{"type":"tool_use","id":"t2","name":"Read","input":{"limit":5}}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"s","uuid":"u3","parentUuid":"u2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"five lines"},{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"total 0"}]}]}}"#,
        );
        let ir = crate::adapter::claude_code::ClaudeCode.parse(raw).unwrap();
        let details = tool_details("claude-code", raw, &ir);
        let tools: Vec<usize> = ir
            .events
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.kind, EventKind::ToolUse | EventKind::FileEdit))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(tools.len(), 2, "both calls must become tool events");
        let d1 = details.get(tools[0]).expect("the first call has details");
        assert_eq!(d1.input.as_ref().unwrap()["command"], "ls -l");
        assert_eq!(d1.output.as_deref(), Some("total 0"), "outputs pair by id");
        let d2 = details.get(tools[1]).expect("the second call has details");
        assert_eq!(d2.input.as_ref().unwrap()["limit"], 5);
        assert_eq!(d2.output.as_deref(), Some("five lines"));
    }

    /// Codex source: the arguments string decodes into structure, and outputs land by call_id.
    #[test]
    fn codex_details_decode_arguments_and_pair_outputs() {
        let raw = concat!(
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/r"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"go"}]}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call","call_id":"c1","name":"shell","arguments":"{\"command\":[\"ls\"]}"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"total 0"}}"#,
        );
        let ir = crate::adapter::codex::Codex.parse(raw).unwrap();
        let details = tool_details("codex", raw, &ir);
        let (i, _) = ir
            .events
            .iter()
            .enumerate()
            .find(|(_, e)| e.kind == EventKind::ToolUse)
            .expect("a tool event is present");
        let d = details.get(i).expect("details are present");
        assert_eq!(d.input.as_ref().unwrap()["command"][0], "ls");
        assert_eq!(d.output.as_deref(), Some("total 0"));
    }

    /// OpenCode source: input and output sit in the state on the tool part's own line.
    #[test]
    fn opencode_details_read_state_in_place() {
        use crate::adapter::Event;
        let raw = concat!(
            r#"{"id":"m1","kind":"message","time_created":1,"data":{"role":"assistant"}}"#,
            "\n",
            r#"{"id":"p1","kind":"part","message_id":"m1","time_created":2,"data":{"type":"tool","tool":"bash","callID":"c1","state":{"status":"completed","input":{"command":"ls"},"output":"total 0","metadata":{}}}}"#,
        );
        let session = Session {
            id: "s".into(),
            runtime: "opencode".into(),
            cwd: None,
            events: vec![Event {
                kind: EventKind::ToolUse,
                text: Some("bash".into()),
                timestamp: None,
                paths: vec![],
                tool: Some("bash".into()),
                line: Some(1),
            }],
        };
        let details = tool_details("opencode", raw, &session);
        let d = details.get(0).expect("details are present");
        assert_eq!(d.input.as_ref().unwrap()["command"], "ls");
        assert_eq!(d.output.as_deref(), Some("total 0"));
    }

    /// An empty output is a fact, not a gap: `Some("")` must pass through the extractor, and the
    /// rendering side must not put a placeholder there.
    #[test]
    fn an_empty_output_survives_as_empty_not_missing() {
        let raw = concat!(
            r#"{"type":"assistant","sessionId":"s","uuid":"u1","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"true"}}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"s","uuid":"u2","parentUuid":"u1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":""}]}}"#,
        );
        let ir = crate::adapter::claude_code::ClaudeCode.parse(raw).unwrap();
        let details = tool_details("claude-code", raw, &ir);
        let (i, _) = ir
            .events
            .iter()
            .enumerate()
            .find(|(_, e)| e.kind == EventKind::ToolUse)
            .unwrap();
        let d = details.get(i).expect("details are present");
        assert_eq!(
            d.output.as_deref(),
            Some(""),
            "an empty output is preserved as Some(\"\")"
        );
        let out = crate::adapter::codex::Codex
            .render_with(&ir, "nid", std::path::Path::new("/r"), &details)
            .unwrap();
        assert!(
            !out.contains("was not carried over"),
            "an empty output is not a gap, so it carries no placeholder: {out}"
        );
    }

    /// An OpenCode call that failed: the body is in `state.error`, and the error status is kept
    /// through to the target side.
    #[test]
    fn an_opencode_error_keeps_its_body_and_status() {
        use crate::adapter::Event;
        let raw = concat!(
            r#"{"id":"m1","kind":"message","time_created":1,"data":{"role":"assistant"}}"#,
            "\n",
            r#"{"id":"p1","kind":"part","message_id":"m1","time_created":2,"data":{"type":"tool","tool":"bash","callID":"c1","state":{"status":"error","input":{"command":"boom"},"error":"exit 1: boom","metadata":{}}}}"#,
        );
        let session = Session {
            id: "s".into(),
            runtime: "opencode".into(),
            cwd: None,
            events: vec![Event {
                kind: EventKind::ToolUse,
                text: Some("bash".into()),
                timestamp: None,
                paths: vec![],
                tool: Some("bash".into()),
                line: Some(1),
            }],
        };
        let details = tool_details("opencode", raw, &session);
        let d = details.get(0).expect("details are present");
        assert!(d.error, "the failed status must survive");
        assert_eq!(d.output.as_deref(), Some("exit 1: boom"));
        let out = crate::adapter::claude_code::ClaudeCode
            .render_with(&session, "nid", std::path::Path::new("/r"), &details)
            .unwrap();
        let result_line = out
            .lines()
            .find(|l| l.contains("tool_result"))
            .expect("a paired output is present");
        let v: serde_json::Value = serde_json::from_str(result_line).unwrap();
        assert_eq!(v["message"]["content"][0]["is_error"], true);
        assert_eq!(v["message"]["content"][0]["content"], "exit 1: boom");
    }

    /// Edit events (FileEdit) consume details too: once an OpenCode edit moves to Codex, the call
    /// pair carries the real arguments and output, and the file-change signal is still there.
    #[test]
    fn a_file_edit_event_carries_details_to_codex() {
        use crate::adapter::Event;
        let raw = concat!(
            r#"{"id":"m1","kind":"message","time_created":1,"data":{"role":"assistant"}}"#,
            "\n",
            r#"{"id":"p1","kind":"part","message_id":"m1","time_created":2,"data":{"type":"tool","tool":"edit","callID":"c1","state":{"status":"completed","input":{"filePath":"/repo/a.rs","old":"x","new":"y"},"output":"edited","metadata":{}}}}"#,
        );
        let session = Session {
            id: "s".into(),
            runtime: "opencode".into(),
            cwd: None,
            events: vec![Event {
                kind: EventKind::FileEdit,
                text: Some("edit".into()),
                timestamp: None,
                paths: vec!["/repo/a.rs".into()],
                tool: Some("edit".into()),
                line: Some(1),
            }],
        };
        let details = tool_details("opencode", raw, &session);
        assert!(
            details.get(0).is_some(),
            "a FileEdit is in the enrichment table too"
        );
        let out = crate::adapter::codex::Codex
            .render_with(&session, "nid", std::path::Path::new("/r"), &details)
            .unwrap();
        let lines: Vec<serde_json::Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let call = lines
            .iter()
            .find(|v| v["payload"]["type"] == "function_call")
            .expect("an edit emits a call pair too");
        assert!(
            call["payload"]["arguments"]
                .as_str()
                .unwrap()
                .contains("/repo/a.rs"),
            "the arguments carry the file path"
        );
        let output = lines
            .iter()
            .find(|v| v["payload"]["type"] == "function_call_output")
            .unwrap();
        assert_eq!(output["payload"]["output"], "edited");
        assert!(
            lines
                .iter()
                .any(|v| v["payload"]["type"] == "patch_apply_end"),
            "the file-change signal must still be present"
        );
        // The Claude target pairs the same way.
        let out = crate::adapter::claude_code::ClaudeCode
            .render_with(&session, "nid", std::path::Path::new("/r"), &details)
            .unwrap();
        assert!(
            out.contains("tool_use"),
            "an edit is a tool_use block on the Claude side: {out}"
        );
        assert!(out.contains("edited"));
    }

    /// One edit in a Codex source = the call (ToolUse) + the receipt (patch_apply_end →
    /// FileEdit): the target side may show the call only once, the receipt leaves nothing but the
    /// change signal, and no phantom with placeholder output is added.
    #[test]
    fn a_codex_patch_receipt_is_not_minted_into_a_second_call() {
        let raw = concat!(
            r#"{"type":"session_meta","payload":{"id":"s","cwd":"/r"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"custom_tool_call","call_id":"c1","name":"apply_patch","input":"*** Begin Patch"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"c1","output":"Done!"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"patch_apply_end","changes":{"/r/a.rs":{}},"success":true}}"#,
        );
        let ir = crate::adapter::codex::Codex.parse(raw).unwrap();
        let details = tool_details("codex", raw, &ir);
        // Claude target: the tool_use appears once, with no placeholder output.
        let out = crate::adapter::claude_code::ClaudeCode
            .render_with(&ir, "nid", std::path::Path::new("/r"), &details)
            .unwrap();
        assert_eq!(out.matches("\"tool_use\"").count(), 1, "{out}");
        assert!(!out.contains("was not carried over"), "{out}");
        // Codex target: one call pair plus one change signal.
        let out = crate::adapter::codex::Codex
            .render_with(&ir, "nid", std::path::Path::new("/r"), &details)
            .unwrap();
        assert_eq!(
            out.matches("\"custom_tool_call\"").count(),
            0,
            "a call is uniformly a function_call"
        );
        assert_eq!(out.matches("\"function_call\"").count(), 1, "{out}");
        assert_eq!(out.matches("patch_apply_end").count(), 1);
        assert!(!out.contains("was not carried over"), "{out}");
        // OpenCode target: a single tool part.
        let out = crate::adapter::opencode::OpenCode
            .render_with(&ir, "ses_x", std::path::Path::new("/r"), &details)
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let tool_parts = v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["parts"].as_array().unwrap())
            .filter(|p| p["type"] == "tool")
            .count();
        assert_eq!(tool_parts, 1, "{out}");
    }

    /// A call that failed is marked failed faithfully at every target: OpenCode's
    /// state.status/error, success=false on the Codex edit signal (Claude's is_error has its own
    /// regression).
    #[test]
    fn a_failed_edit_is_marked_failed_at_every_target() {
        use crate::adapter::{Event, ToolDetail, ToolDetails};
        let session = Session {
            id: "s".into(),
            runtime: "claude-code".into(),
            cwd: None,
            events: vec![Event {
                kind: EventKind::FileEdit,
                text: Some("Edit".into()),
                timestamp: None,
                paths: vec!["/r/a.rs".into()],
                tool: Some("Edit".into()),
                line: None,
            }],
        };
        let mut details = ToolDetails::default();
        details.insert(
            0,
            ToolDetail {
                input: Some(serde_json::json!({"file_path": "/r/a.rs"})),
                output: Some("String to replace not found".into()),
                error: true,
            },
        );
        let out = crate::adapter::opencode::OpenCode
            .render_with(&session, "ses_x", std::path::Path::new("/r"), &details)
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let part = &v["messages"][0]["parts"][0];
        assert_eq!(part["state"]["status"], "error");
        assert_eq!(part["state"]["error"], "String to replace not found");
        assert_eq!(part["state"]["output"], "");
        let out = crate::adapter::codex::Codex
            .render_with(&session, "nid", std::path::Path::new("/r"), &details)
            .unwrap();
        let patch_line: serde_json::Value = out
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .find(|v: &serde_json::Value| v["payload"]["type"] == "patch_apply_end")
            .expect("the edit signal is present");
        assert_eq!(patch_line["payload"]["success"], false);
    }

    /// A source file that is not line-per-JSON (the export form) yields an empty table, and
    /// installation is unaffected.
    #[test]
    fn a_non_jsonl_source_yields_an_empty_table() {
        use crate::adapter::Event;
        let session = Session {
            id: "s".into(),
            runtime: "opencode".into(),
            cwd: None,
            events: vec![Event {
                kind: EventKind::ToolUse,
                text: Some("bash".into()),
                timestamp: None,
                paths: vec![],
                tool: Some("bash".into()),
                line: Some(1),
            }],
        };
        let details = tool_details("opencode", "{\n  \"info\": {}\n}", &session);
        assert!(details.is_empty());
    }
}
