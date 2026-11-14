//! Gemini CLI writes a message again, whole, each time it changes: when its tool calls are
//! scheduled, run or answered, and when its usage arrives. Published line for line, every tool step
//! would publish the whole message again. This publishes each part of a message once.
//!
//! It runs on lines protection already passed, whole: a part is compared as published, never
//! checked on its own, so a secret split across two copies of a message is judged where it is
//! whole. Within the lines a snapshot adds, only the last copy of a message is published, less what
//! the lines published before it hold of that message, in their published form; an earlier copy,
//! or one that adds nothing, is published as a stub naming only the message. A part that changed
//! after it was published is not published again; the VIEW follows the record.
//!
//! Every line a snapshot keeps has a published line, a stub included, so the public LOG names the
//! records the source LOG keeps one for one: the next snapshot finds the lines published before it
//! by position, and reads what they hold from them, never from the source lines they came from.

use crate::domain::{storage, transcript::Envelope};
use anyhow::Result;
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};

const SOURCE: &str = "gemini-cli";

/// What earlier published lines of one message hold.
#[derive(Default)]
struct Parts {
    text: bool,
    thoughts: usize,
    calls: BTreeSet<String>,
    results: BTreeSet<String>,
}

type Key = (String, String);

/// Whether any of `records` is a Gemini CLI record, so the lines need [`publish_once`].
pub(super) fn applies<'a>(mut records: impl Iterator<Item = &'a Envelope>) -> bool {
    records.any(|record| record.source == SOURCE)
}

/// Rewrites `public_lines`, in LOG order, so each part of a Gemini CLI message is published once.
/// `fresh` marks the lines this snapshot adds, by position past the public parent's; every other
/// line is published already, as it stands in `public_lines`, and only tells what its message
/// holds.
pub(super) fn publish_once(public_lines: &mut [Option<String>], fresh: &[bool]) -> Result<()> {
    let records = public_lines
        .iter()
        .map(|line| {
            line.as_deref()
                .map(storage::parse_legacy_envelope_line)
                .transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    let key = |record: &Envelope| -> Option<Key> {
        (record.source == SOURCE)
            .then(|| record.content["id"].as_str())
            .flatten()
            .map(|id| (record.session_id.clone(), id.to_owned()))
    };
    let mut last = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        if let Some(record) = record
            && fresh[index]
            && let Some(key) = key(record)
        {
            last.insert(key, index);
        }
    }
    let mut held: HashMap<Key, Parts> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        let Some(record) = record.as_ref().filter(|record| record.source == SOURCE) else {
            continue;
        };
        if !fresh[index] {
            hold(&mut held, &record.session_id, &record.content);
            continue;
        }
        let content = if key(record).is_some_and(|key| last.get(&key) != Some(&index)) {
            None
        } else {
            new_parts(&held, &record.session_id, &record.content)
        }
        .unwrap_or_else(|| stub(&record.content));
        hold(&mut held, &record.session_id, &content);
        if content != record.content {
            public_lines[index] = Some(storage::envelope_line(&Envelope {
                source: record.source.clone(),
                session_id: record.session_id.clone(),
                object_hash: crate::domain::transcript::object_hash(&content),
                content,
            }));
        }
    }
    Ok(())
}

/// A record naming only its message, for a copy whose parts are published elsewhere.
fn stub(content: &Value) -> Value {
    if content.get("$patch").is_some() {
        return serde_json::json!({"$patch": {"updates": []}});
    }
    let mut stub = Map::new();
    for field in ["id", "timestamp", "type"] {
        if let Some(value) = content.get(field) {
            stub.insert(field.into(), value.clone());
        }
    }
    Value::Object(stub)
}

/// Adds what a published `content` holds to `held`.
fn hold(held: &mut HashMap<Key, Parts>, session: &str, content: &Value) {
    if let Some(patch) = content.get("$patch") {
        for update in patch["updates"].as_array().into_iter().flatten() {
            let Some(id) = update["id"].as_str() else {
                continue;
            };
            let parts = held.entry((session.to_owned(), id.to_owned())).or_default();
            for call in update["toolCalls"].as_array().into_iter().flatten() {
                if let Some(call) = call["id"].as_str() {
                    parts.results.insert(call.to_owned());
                }
            }
        }
        return;
    }
    let Some(id) = content["id"].as_str() else {
        return;
    };
    let parts = held.entry((session.to_owned(), id.to_owned())).or_default();
    parts.text |= has_text(&content["content"]);
    parts.thoughts = parts
        .thoughts
        .max(content["thoughts"].as_array().map_or(0, Vec::len));
    for call in content["toolCalls"].as_array().into_iter().flatten() {
        if let Some(call_id) = call["id"].as_str() {
            parts.calls.insert(call_id.to_owned());
            if !call["result"].is_null() {
                parts.results.insert(call_id.to_owned());
            }
        }
    }
}

fn has_text(content: &Value) -> bool {
    match content {
        Value::String(text) => !text.is_empty(),
        Value::Array(parts) => !parts.is_empty(),
        Value::Null => false,
        _ => true,
    }
}

/// What `content` adds to what earlier published lines of its message hold, or `None` when it
/// adds nothing.
fn new_parts(held: &HashMap<Key, Parts>, session: &str, content: &Value) -> Option<Value> {
    let none = Parts::default();
    let parts_of = |id: &str| {
        held.get(&(session.to_owned(), id.to_owned()))
            .unwrap_or(&none)
    };
    if let Some(patch) = content.get("$patch") {
        let updates: Vec<Value> = patch["updates"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|update| {
                let parts = parts_of(update["id"].as_str()?);
                let results: Vec<Value> = update["toolCalls"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|call| {
                        call["id"]
                            .as_str()
                            .is_some_and(|id| !parts.results.contains(id))
                    })
                    .cloned()
                    .collect();
                (!results.is_empty()).then(|| {
                    let mut update = update.clone();
                    update["toolCalls"] = Value::Array(results);
                    update
                })
            })
            .collect();
        return (!updates.is_empty()).then(|| serde_json::json!({"$patch": {"updates": updates}}));
    }
    let Some(id) = content["id"].as_str() else {
        return Some(content.clone());
    };
    let parts = parts_of(id);
    let record = content.as_object()?;
    match record.get("type").and_then(Value::as_str) {
        Some("user") => (!parts.text).then(|| content.clone()),
        Some("gemini") => {
            let mut added: Map<String, Value> = record
                .iter()
                .filter(|(key, _)| !matches!(key.as_str(), "content" | "thoughts" | "toolCalls"))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            let mut new = false;
            if !parts.text
                && let Some(text) = record.get("content").filter(|text| has_text(text))
            {
                added.insert("content".into(), text.clone());
                new = true;
            }
            let thoughts: Vec<Value> = record
                .get("thoughts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .skip(parts.thoughts)
                .cloned()
                .collect();
            if !thoughts.is_empty() {
                added.insert("thoughts".into(), Value::Array(thoughts));
                new = true;
            }
            let calls: Vec<Value> = record
                .get("toolCalls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|call| {
                    let call_id = call["id"].as_str()?;
                    if !parts.calls.contains(call_id) {
                        return Some(call.clone());
                    }
                    if call["result"].is_null() || parts.results.contains(call_id) {
                        return None;
                    }
                    // The call went out before; only its result is new.
                    let mut answered = Map::new();
                    for field in ["id", "name", "status", "result"] {
                        if let Some(value) = call.get(field) {
                            answered.insert(field.into(), value.clone());
                        }
                    }
                    Some(Value::Object(answered))
                })
                .collect();
            if !calls.is_empty() {
                added.insert("toolCalls".into(), Value::Array(calls));
                new = true;
            }
            new.then_some(Value::Object(added))
        }
        _ => Some(content.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(content: Value) -> Option<String> {
        Some(storage::envelope_line(&Envelope {
            source: SOURCE.into(),
            session_id: format!("agit-{}", "e".repeat(40)),
            object_hash: crate::domain::transcript::object_hash(&content),
            content,
        }))
    }

    fn contents(lines: &[Option<String>]) -> Vec<Option<Value>> {
        lines
            .iter()
            .map(|line| {
                line.as_deref()
                    .map(|line| storage::parse_legacy_envelope_line(line).unwrap().content)
            })
            .collect()
    }

    /// Each part of a message is published once: the copy a later snapshot adds publishes only the
    /// call it answers, and the copies before the last within one snapshot, like a patch that adds
    /// nothing, are stubs naming only their message. A part that changed after it was published is
    /// not published again. Publishing every copy repeats the reply and its calls per tool step;
    /// publishing only the first loses the results.
    #[test]
    fn each_part_of_a_message_is_published_once() {
        let reply = |calls: Value| json!({"id":"m2","type":"gemini","content":[{"text":"Reading"}],"thoughts":[{"subject":"plan"}],"toolCalls":calls});
        let call = |id: &str, result: Value| json!({"id":id,"name":"read_file","args":{"file_path":"a"},"result":result});
        let answered = json!([{"functionResponse":{"response":{"output":"A"}}}]);
        // An earlier snapshot published the reply with its call scheduled.
        let mut lines = vec![
            line(json!({"id":"m1","type":"user","content":"Read a"})),
            line(reply(json!([call("c1", Value::Null)]))),
            line(reply(json!([call("c1", answered.clone())]))),
            line(
                json!({"id":"m2","type":"gemini","content":[{"text":"Changed"}],"thoughts":[{"subject":"plan"}],"toolCalls":[call("c1", answered.clone()), call("c2", Value::Null)]}),
            ),
            line(
                json!({"$patch":{"updates":[{"id":"m2","toolCalls":[{"id":"c2","result":answered.clone()}]}]}}),
            ),
            line(
                json!({"$patch":{"updates":[{"id":"m2","toolCalls":[{"id":"c2","result":answered.clone()}]}]}}),
            ),
        ];
        publish_once(&mut lines, &[false, false, true, true, true, true]).unwrap();
        let published = contents(&lines);
        assert_eq!(published[0].as_ref().unwrap()["content"], "Read a");
        assert!(published[1].is_some(), "a published line stays");
        assert_eq!(
            published[2],
            Some(json!({"id":"m2","type":"gemini"})),
            "an earlier copy within the snapshot names only its message"
        );
        assert_eq!(
            published[3],
            Some(json!({"id":"m2","type":"gemini","toolCalls":[
                {"id":"c1","name":"read_file","result":answered},
                call("c2", Value::Null)
            ]}))
        );
        assert_eq!(
            published[4],
            Some(
                json!({"$patch":{"updates":[{"id":"m2","toolCalls":[{"id":"c2","result":answered}]}]}})
            )
        );
        assert_eq!(
            published[5],
            Some(json!({"$patch":{"updates":[]}})),
            "a result already published"
        );
    }
}
