//! Materialized histories must carry usable identifiers even without the original secret dictionary.

use crate::adapter::protocol_ids::{Kind, layout};
use crate::domain::privacy::detector::placeholder::token_segments;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

fn invalid(text: &str, kind: Kind) -> bool {
    kind.too_long(text) || token_segments(text).next().is_some()
}

fn wrapped_carrier(value: &Value) -> bool {
    layout(value).encoded.iter().any(|(pointer, _)| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .is_some_and(|text| {
                token_segments(text)
                    .next()
                    .is_some_and(|(start, end, _)| start == 0 && end == text.len())
            })
    })
}

/// Identifier aliases precede best-effort hydration so a worker deadline cannot restore one
/// half of a call pair while leaving the other half as an unresolved dictionary token. A complete
/// encoded carrier defers this pass until hydration exposes its schema-owned fields.
#[cfg(any(feature = "cli", test))]
pub(crate) fn repair_saved(saved: &str) -> crate::Result<String> {
    let mut envelopes = saved
        .split_inclusive('\n')
        .map(crate::domain::storage::parse_envelope_line)
        .collect::<crate::Result<Vec<_>>>()?;
    let native: String = envelopes
        .iter()
        .map(|envelope| format!("{}\n", envelope.content))
        .collect();
    let repaired = repair(&native)?;
    if repaired == native {
        return Ok(saved.into());
    }
    let mut output = String::new();
    for (envelope, line) in envelopes.iter_mut().zip(repaired.lines()) {
        envelope.content = serde_json::from_str(line)?;
        envelope.object_hash = crate::domain::transcript::object_hash(&envelope.content);
        output.push_str(&crate::domain::storage::envelope_line(envelope));
    }
    Ok(output)
}

#[cfg(any(feature = "cli", test))]
pub(crate) fn needs_repair(content: &str) -> bool {
    content.lines().any(|line| {
        let Ok(mut value) = serde_json::from_str::<Value>(line) else {
            return false;
        };
        let mut found = false;
        layout(&value).map(&mut value, &mut |text, kind| {
            found |= invalid(text, kind);
            text.into()
        });
        found
    })
}

/// Reserve every existing identifier before assigning aliases, including identifiers later in
/// the transcript. Equal references share an alias; arguments and outputs are never rewritten.
pub(crate) fn repair(content: &str) -> crate::Result<String> {
    let mut occupied = BTreeSet::new();
    let mut pending = BTreeSet::new();
    let mut deferred = false;
    for line in content.lines() {
        let Ok(mut value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        deferred |= wrapped_carrier(&value);
        layout(&value).map(&mut value, &mut |text, kind| {
            occupied.insert(text.to_owned());
            if invalid(text, kind) {
                pending.insert((kind.prefix(), text.to_owned()));
            }
            text.into()
        });
    }
    // A complete encoded carrier can hide a call ID until privacy hydration. Deferring all
    // aliases keeps visible result IDs aligned with those hidden fields on the second pass.
    if deferred {
        return Ok(content.to_owned());
    }
    if pending.is_empty() {
        return Ok(content.to_owned());
    }
    let mut aliases = BTreeMap::new();
    for (prefix, original) in pending {
        let mut salt = 0_u64;
        loop {
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(serde_json::to_vec(&(prefix, &original, salt))?);
            let uuid = uuid::Uuid::from_bytes(digest[..16].try_into().expect("UUID digest width"));
            let alias = if prefix.is_empty() {
                uuid.to_string()
            } else {
                format!("{prefix}{}", uuid.simple())
            };
            if occupied.insert(alias.clone()) {
                aliases.insert((prefix, original), alias);
                break;
            }
            salt += 1;
        }
    }
    let mut output = String::with_capacity(content.len());
    for line in content.split_inclusive('\n') {
        let Ok(mut value) = serde_json::from_str::<Value>(line) else {
            output.push_str(line);
            continue;
        };
        let mut changed = false;
        layout(&value).map(&mut value, &mut |text, kind| match aliases
            .get(&(kind.prefix(), text.to_owned()))
        {
            Some(alias) => {
                changed = true;
                alias.clone()
            }
            None => text.into(),
        });
        if changed {
            output.push_str(&serde_json::to_string(&value)?);
            if line.ends_with('\n') {
                output.push('\n');
            }
        } else {
            output.push_str(line);
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TOKEN: &str = "{{AGIT_SECRET_V2:11111111-1111-4111-8111-111111111111:22222222-2222-4222-8222-222222222222}}";

    fn lines(records: &[Value]) -> String {
        records.iter().map(|record| format!("{record}\n")).collect()
    }

    /// Replay requires bounded, collision-free call IDs across both ordinary and compacted
    /// history. Replacing text globally or minting an alias per occurrence breaks this contract.
    #[test]
    fn codex_materialization_repairs_call_pairs_and_preserves_tool_contents() {
        for (call, result) in [
            ("function_call", "function_call_output"),
            ("custom_tool_call", "custom_tool_call_output"),
            ("local_shell_call", "local_shell_call_output"),
        ] {
            let call =
                json!({"type":call,"call_id":TOKEN,"name":"shell","arguments":TOKEN,"input":TOKEN});
            let result = json!({"type":result,"call_id":TOKEN,"output":TOKEN});
            let raw = lines(&[
                json!({"type":"response_item","payload":call}),
                json!({"type":"response_item","payload":result}),
                json!({"type":"compacted","payload":{"replacement_history":[call,result]}}),
            ]);
            let first = repair(&raw).unwrap();
            let first_row: Value = serde_json::from_str(first.lines().next().unwrap()).unwrap();
            let collision = first_row["payload"]["call_id"].as_str().unwrap();
            let boundary = "x".repeat(64);
            let raw = raw
                + &lines(&[
                    json!({"type":"response_item","payload":{"type":"function_call_output","call_id":collision,"output":"existing identity"}}),
                    json!({"type":"response_item","payload":{"type":"function_call_output","call_id":boundary,"output":"valid boundary"}}),
                    json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"x".repeat(65),"output":"oversized identity"}}),
                ]);
            assert!(needs_repair(&raw));
            let saved = crate::domain::transcript::wrap_lines(
                &raw,
                "codex",
                &format!("agit-{}", "a".repeat(40)),
            );
            let prepared = repair_saved(&saved).unwrap();
            let prepared_native = crate::domain::transcript::unwrap_strict(&prepared).unwrap();
            assert!(!needs_repair(&prepared_native));
            assert!(
                prepared_native.contains(TOKEN),
                "tool content still awaits dictionary hydration"
            );
            let localized = crate::adapter::get("codex")
                .unwrap()
                .localize(
                    &raw,
                    "aaaaaaaa-0000-4000-8000-000000000001",
                    std::path::Path::new("/workspace"),
                )
                .unwrap();
            assert!(!needs_repair(&localized));
            assert_eq!(repair(&localized).unwrap(), localized);
            let records: Vec<Value> = localized
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let rows: Vec<_> = records
                .iter()
                .filter(|row| row["type"] == "response_item")
                .collect();
            let alias = rows[0]["payload"]["call_id"].as_str().unwrap();
            assert!(alias.len() <= 64);
            assert_ne!(alias, collision);
            assert_eq!(rows[1]["payload"]["call_id"], alias);
            assert_eq!(rows[0]["payload"]["arguments"], TOKEN);
            assert_eq!(rows[1]["payload"]["output"], TOKEN);
            let compact = records
                .iter()
                .find(|row| row["type"] == "compacted")
                .unwrap();
            for item in compact["payload"]["replacement_history"]
                .as_array()
                .unwrap()
            {
                assert_eq!(item["call_id"], alias);
            }
            assert_eq!(rows[2]["payload"]["call_id"], collision);
            assert_eq!(rows[3]["payload"]["call_id"], boundary);
            assert!(rows[4]["payload"]["call_id"].as_str().unwrap().len() <= 64);
        }
    }

    /// Provider-specific linkage and encoded tool-call arrays must share aliases with results,
    /// while surrounding text and valid record identities retain their original values.
    #[test]
    fn native_aliases_pair_across_provider_fields_and_encoded_json() {
        let fixtures = [
            (
                "claude-code",
                vec![
                    json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":TOKEN,"name":"Read","input":{"id":TOKEN}}]}}),
                    json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":TOKEN,"content":TOKEN}]}}),
                ],
                "/message/content/0/id",
                "/message/content/0/tool_use_id",
            ),
            (
                "openclaw",
                vec![
                    json!({"type":"message","id":"entry1","message":{"role":"assistant","content":[{"type":"toolCall","id":TOKEN,"name":"read","arguments":{"id":TOKEN}}]}}),
                    json!({"type":"message","id":"entry2","parentId":"entry1","message":{"role":"toolResult","toolCallId":TOKEN,"content":TOKEN}}),
                ],
                "/message/content/0/id",
                "/message/toolCallId",
            ),
            (
                "workbuddy",
                vec![
                    json!({"type":"function_call","sessionId":"session","callId":TOKEN,"name":"read","arguments":{"id":TOKEN}}),
                    json!({"type":"function_call_result","sessionId":"session","callId":TOKEN,"output":TOKEN}),
                ],
                "/callId",
                "/callId",
            ),
        ];
        for (runtime, records, call, result) in fixtures {
            let repaired = repair(&lines(&records)).unwrap();
            let rows: Vec<Value> = repaired
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(rows[0].pointer(call), rows[1].pointer(result), "{runtime}");
            assert_ne!(rows[0].pointer(call).unwrap(), TOKEN, "{runtime}");
            assert!(
                rows[0].to_string().contains(TOKEN),
                "{runtime}: tool input stays opaque"
            );
            assert!(
                rows[1].to_string().contains(TOKEN),
                "{runtime}: tool output stays opaque"
            );
        }
        let hermes = lines(&[
            json!({"type":"hermes_message","data":{"role":"assistant","session_id":"session","tool_calls":json!([{"id":TOKEN,"function":{"name":"read","arguments":TOKEN}}]).to_string()}}),
            json!({"type":"hermes_message","data":{"role":"tool","session_id":"session","tool_call_id":TOKEN,"content":TOKEN}}),
        ]);
        let repaired = repair(&hermes).unwrap();
        let rows: Vec<Value> = repaired
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let calls: Value =
            serde_json::from_str(rows[0]["data"]["tool_calls"].as_str().unwrap()).unwrap();
        assert_eq!(calls[0]["id"], rows[1]["data"]["tool_call_id"]);
        assert_ne!(calls[0]["id"], TOKEN);
        assert_eq!(calls[0]["function"]["arguments"], TOKEN);
        assert_eq!(rows[1]["data"]["content"], TOKEN);
        assert!(!needs_repair(&repaired));
    }
}
