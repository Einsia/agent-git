//! Native content decoding shared by adapters without changing the stored records.

use super::{Event, EventKind, OpenCall, Session, ToolDetail, ToolDetails};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| {
                matches!(
                    block["type"].as_str(),
                    Some("text" | "input_text" | "output_text")
                )
                .then(|| block["text"].as_str())
                .flatten()
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) if value["type"] == "text" => {
            value["text"].as_str().unwrap_or("").to_owned()
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub(super) fn timestamp(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_owned());
    }
    let millis = value.as_i64()?;
    chrono::DateTime::from_timestamp_millis(millis).map(|time| time.to_rfc3339())
}

pub(super) fn millis(value: Option<&str>) -> i64 {
    value
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map(|time| time.timestamp_millis())
        .unwrap_or(0)
}

#[derive(Clone)]
pub(super) struct Call {
    pub line: usize,
    pub id: String,
    pub name: String,
    pub input: Value,
}

pub(super) struct Output {
    pub id: String,
    pub text: String,
    pub error: bool,
}

pub(super) fn input(value: &Value) -> Value {
    value
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or_else(|| value.clone())
}

pub(super) fn tool_event(call: &Call, timestamp: Option<String>) -> Event {
    let paths = ["file_path", "path", "filePath"]
        .into_iter()
        .find_map(|key| call.input[key].as_str())
        .map(str::to_owned)
        .into_iter()
        .collect();
    Event {
        kind: if matches!(
            call.name.as_str(),
            "Write" | "Edit" | "write_file" | "patch" | "write" | "edit"
        ) {
            EventKind::FileEdit
        } else {
            EventKind::ToolUse
        },
        text: None,
        timestamp,
        paths,
        tool: Some(call.name.clone()),
        line: Some(call.line),
    }
}

pub(super) fn details(
    session: &Session,
    calls: &[Call],
    outputs: &[Output],
    include_output: bool,
) -> ToolDetails {
    let mut output_by_id = BTreeMap::new();
    for output in outputs {
        output_by_id
            .entry(&output.id)
            .and_modify(|value| *value = None)
            .or_insert(Some(output));
    }
    let mut calls_by_line = BTreeMap::<usize, Vec<&Call>>::new();
    for call in calls {
        calls_by_line.entry(call.line).or_default().push(call);
    }
    let mut positions = BTreeMap::<usize, usize>::new();
    let mut result = ToolDetails::default();
    for (index, event) in session.events.iter().enumerate() {
        if !matches!(event.kind, EventKind::ToolUse | EventKind::FileEdit) {
            continue;
        }
        let Some(line) = event.line else { continue };
        let position = positions.entry(line).or_default();
        if let Some(call) = calls_by_line
            .get(&line)
            .and_then(|calls| calls.get(*position))
            .copied()
        {
            let output = output_by_id.get(&call.id).copied().flatten();
            result.insert(
                index,
                ToolDetail {
                    input: Some(call.input.clone()),
                    output: if include_output {
                        output.map(|output| output.text.clone())
                    } else {
                        None
                    },
                    error: output.is_some_and(|output| output.error),
                },
            );
        }
        *position += 1;
    }
    result
}

pub(super) fn open_calls(calls: &[Call], outputs: &[Output], record: &str) -> Vec<OpenCall> {
    let closed: BTreeSet<_> = outputs.iter().map(|output| &output.id).collect();
    calls
        .iter()
        .filter(|call| !closed.contains(&call.id))
        .map(|call| OpenCall {
            line: call.line,
            call_id: call.id.clone(),
            record: record.into(),
            name: call.name.clone(),
        })
        .collect()
}

pub(super) fn remint(id: &str, old: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{}:{id}{old}", id.len()).as_bytes());
    uuid::Uuid::from_bytes(digest[..16].try_into().expect("UUID digest width")).to_string()
}
