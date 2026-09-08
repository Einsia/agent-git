//! Renderable IR from a saved envelope sequence without guessing its native format.

use crate::Result;
use crate::adapter::{self, Event, EventKind, Session};
use crate::domain::{storage, transcript::Envelope};
use anyhow::Context;
use std::collections::BTreeMap;

#[derive(Default)]
struct NativeSession {
    raw: String,
    positions: Vec<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct SourceKey {
    pub source: String,
    session: String,
    native_session: Option<String>,
}

pub(crate) fn source_key(envelope: &Envelope) -> SourceKey {
    // A logical AgentGit session can contain copies of distinct native database sessions.
    // Reused native message identifiers cannot correlate across those source identities.
    let native_session = if envelope.source == "opencode" {
        let field = if envelope.content["kind"] == "opencode.meta" {
            "id"
        } else {
            "session_id"
        };
        envelope.content[field]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    } else {
        None
    };
    SourceKey {
        source: envelope.source.clone(),
        session: envelope.session_id.clone(),
        native_session,
    }
}

/// Native parsers need records from the same source session to correlate messages and parts.
/// Their line coordinates are mapped back to the selected sequence before rendering, so grouping
/// cannot reorder interleaved history or fetch context excluded from that sequence.
pub fn parse(envelopes: &str) -> Result<Session> {
    let mut groups: BTreeMap<SourceKey, NativeSession> = BTreeMap::new();
    let mut events = Vec::new();
    for (position, line) in envelopes.split_inclusive('\n').enumerate() {
        let envelope = storage::parse_envelope_line(line)?;
        let content = &envelope.content;
        if content["type"] == "user"
            && content["agit"] == "merge_summary"
            && content["message"]["role"] == "user"
            && let Some(text) = content["message"]["content"].as_str()
        {
            adapter::get(&envelope.source).with_context(|| {
                format!(
                    "cannot render saved transcript source `{}`",
                    envelope.source
                )
            })?;
            // AgentGit summaries retain their own message schema even when their envelope names
            // a native runtime; native parser dispatch must not discard that selected content.
            events.push(Event::text(EventKind::UserPrompt, text, None).at_line(position));
            continue;
        }
        let group = groups.entry(source_key(&envelope)).or_default();
        group
            .raw
            .push_str(&serde_json::to_string(&envelope.content)?);
        group.raw.push('\n');
        group.positions.push(position);
    }

    for (key, group) in groups {
        let parsed = adapter::get(&key.source)
            .with_context(|| format!("cannot render saved transcript source `{}`", key.source))?
            .parse(&group.raw)?;
        for mut event in parsed.events {
            let line = event
                .line
                .context("saved transcript event has no source coordinate")?;
            let position = group
                .positions
                .get(line)
                .context("saved transcript event has an invalid source coordinate")?;
            event.line = Some(*position);
            events.push(event);
        }
    }
    events.sort_by_key(|event| event.line);
    Ok(Session {
        id: String::new(),
        runtime: "agentgit-view".into(),
        cwd: None,
        events,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::transcript;
    use serde_json::{Value, json};

    fn wrap(source: &str, claim: char, content: Value) -> String {
        transcript::wrap_lines(
            &content.to_string(),
            source,
            &format!("agit-{}", claim.to_string().repeat(40)),
        )
    }

    #[test]
    fn interleaved_native_records_keep_view_order_and_session_correlation() {
        let message =
            |role: &str| json!({"kind":"message", "id":"shared-native-id", "data":{"role":role}});
        let part = |text: &str| json!({"kind":"part", "message_id":"shared-native-id", "data":{"type":"text", "text":text}});
        let text = [
            wrap("opencode", 'a', message("user")),
            wrap("opencode", 'b', message("assistant")),
            wrap("claude-code", 'c', json!({"type":"user", "message":{"role":"user", "content":"claude"}})),
            wrap("opencode", 'a', part("open-user")),
            wrap("codex", 'd', json!({"type":"response_item", "payload":{"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"codex"}]}})),
            wrap("opencode", 'b', part("open-assistant")),
        ].concat();
        let parsed = parse(&text).unwrap();
        let actual: Vec<_> = parsed
            .events
            .iter()
            .map(|event| {
                (
                    event.line.unwrap(),
                    event.kind,
                    event.text.as_deref().unwrap(),
                )
            })
            .collect();
        use adapter::EventKind::{AssistantReply, UserPrompt};
        assert_eq!(
            actual,
            [
                (2, UserPrompt, "claude"),
                (3, UserPrompt, "open-user"),
                (4, AssistantReply, "codex"),
                (5, AssistantReply, "open-assistant")
            ]
        );
    }

    #[test]
    fn unknown_source_is_not_reinterpreted_as_another_runtime() {
        let text = wrap(
            "future-runtime",
            'a',
            json!({"type":"user", "message":{"role":"user", "content":"selected"}}),
        );
        assert!(
            parse(&text)
                .unwrap_err()
                .to_string()
                .contains("cannot render saved transcript source")
        );
    }

    #[test]
    fn native_session_identity_separates_reused_message_ids_inside_a_logical_session() {
        let text = [
            wrap("opencode", 'a', json!({"kind":"opencode.meta", "id":"first"})),
            wrap("opencode", 'a', json!({"kind":"message", "session_id":"first", "id":"shared", "data":{"role":"user"}})),
            wrap("opencode", 'a', json!({"kind":"message", "session_id":"second", "id":"shared", "data":{"role":"assistant"}})),
            wrap("opencode", 'a', json!({"kind":"part", "session_id":"first", "message_id":"shared", "data":{"type":"text", "text":"prompt"}})),
            wrap("opencode", 'a', json!({"kind":"part", "session_id":"second", "message_id":"shared", "data":{"type":"text", "text":"reply"}})),
        ].concat();
        let parsed = parse(&text).unwrap();
        assert_eq!(
            parsed
                .events
                .iter()
                .map(|event| (event.line, event.kind))
                .collect::<Vec<_>>(),
            [
                (Some(3), adapter::EventKind::UserPrompt),
                (Some(4), adapter::EventKind::AssistantReply),
            ]
        );
    }

    #[test]
    fn same_envelope_blocks_keep_native_order() {
        let text = wrap(
            "claude-code",
            'a',
            json!({"type":"assistant", "message":{"role":"assistant", "content":[
                {"type":"text", "text":"before-call"},
                {"type":"tool_use", "id":"call", "name":"Bash", "input":{"command":"true"}},
                {"type":"text", "text":"after-call"}
            ]}}),
        );
        let parsed = parse(&text).unwrap();
        use adapter::EventKind::{AssistantReply, ToolUse};
        assert_eq!(
            parsed.events.iter().map(|e| e.kind).collect::<Vec<_>>(),
            [AssistantReply, ToolUse, AssistantReply]
        );
        assert!(parsed.events.iter().all(|e| e.line == Some(0)));
        assert_eq!(parsed.events[0].text.as_deref(), Some("before-call"));
        assert_eq!(parsed.events[2].text.as_deref(), Some("after-call"));
    }

    #[test]
    fn interleaving_does_not_separate_opencode_compaction_metadata() {
        let text = [
            wrap("opencode", 'a', json!({"kind":"message", "id":"boundary", "data":{"role":"user"}})),
            wrap("opencode", 'a', json!({"kind":"part", "message_id":"boundary", "data":{"type":"compaction"}})),
            wrap("claude-code", 'b', json!({"type":"user", "message":{"role":"user", "content":"interleaved"}})),
            wrap("opencode", 'a', json!({"kind":"message", "id":"summary", "data":{"role":"assistant", "mode":"compaction"}})),
            wrap("opencode", 'a', json!({"kind":"part", "message_id":"summary", "data":{"type":"text", "text":"selected-summary"}})),
        ].concat();
        let parsed = parse(&text).unwrap();
        assert_eq!(parsed.events.len(), 2);
        assert_eq!(parsed.events[0].kind, adapter::EventKind::CompactSummary);
        assert_eq!(parsed.events[0].line, Some(1));
        assert_eq!(parsed.events[0].text.as_deref(), Some("selected-summary"));
        assert_eq!(parsed.events[1].line, Some(2));
        assert_eq!(parsed.events[1].text.as_deref(), Some("interleaved"));
    }
}
