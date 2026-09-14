use super::*;
use crate::adapter::EventKind;
use crate::domain::redact::{Persona, Redactor};
use crate::rc::tail::{TailedLine, Tailer};
use serde_json::{Value, json};
use std::io::Write;

fn metadata(mode: Option<Value>) -> String {
    let mut payload =
        json!({"id":"native-thread","originator":"codex_cli_rs","cli_version":"0.151.0"});
    if let Some(mode) = mode {
        payload["history_mode"] = mode;
    }
    format!("{}\n", json!({"type":"session_meta","payload":payload}))
}

fn native_user(mode: HistoryMode, identity: &str, text: &str) -> Value {
    let payload = match mode {
        HistoryMode::Legacy => json!({"type":"user_message","client_id":identity,"message":text}),
        HistoryMode::Paginated => json!({"type":"item_completed","turn_id":"native-turn","item":{
            "type":"UserMessage","id":"native-ui-item","client_id":identity,
            "content":[{"type":"text","text":text,"text_elements":[]}]}}),
        HistoryMode::Model => unreachable!(),
    };
    json!({"type":"event_msg","timestamp":"2026-09-14T00:00:00Z","payload":payload})
}

fn model_user(text: &str) -> Value {
    json!({"type":"response_item","payload":{"type":"message","role":"user",
        "id":"independent-model-item","content":[{"type":"input_text","text":text}]}})
}

#[test]
fn metadata_defaults_only_native_legacy_and_preserves_unknown_model_histories() {
    for (value, expected) in [
        (None, HistoryMode::Legacy),
        (Some(json!("legacy")), HistoryMode::Legacy),
        (Some(json!("paginated")), HistoryMode::Paginated),
        (Some(json!("future")), HistoryMode::Model),
        (Some(Value::Null), HistoryMode::Model),
        (Some(json!({})), HistoryMode::Model),
    ] {
        assert_eq!(
            read_prefix(metadata(value).as_bytes()),
            Header::Ready(expected)
        );
    }
    let imported = json!({"type":"session_meta","payload":{"id":"imported","originator":"agit","cli_version":"0.1.2"}});
    assert_eq!(
        read_prefix(format!("{imported}\n").as_bytes()),
        Header::Ready(HistoryMode::Model)
    );
    assert_eq!(
        read_prefix(b"{\"type\":\"session_meta\"".as_slice()),
        Header::Pending
    );
    assert_eq!(
        read_prefix(b"invalid\n".as_slice()),
        Header::Ready(HistoryMode::Model)
    );
    assert_eq!(
        read_prefix(std::io::repeat(b'x')),
        Header::Ready(HistoryMode::Model)
    );
}

#[test]
fn canonical_projection_keeps_human_text_and_leaves_archived_model_parsing_unchanged() {
    let identity = uuid::Uuid::new_v4().to_string();
    for mode in [HistoryMode::Legacy, HistoryMode::Paginated] {
        let model = model_user("same user message");
        let user = native_user(mode, &identity, "same user message");
        let stored = crate::adapter::codex::parse_records([(0, &model), (1, &user)]);
        assert_eq!(
            stored
                .events
                .iter()
                .filter(|event| event.kind == EventKind::UserPrompt)
                .count(),
            1
        );
        assert!(
            events(&model, mode)
                .iter()
                .all(|event| event.kind != EventKind::UserPrompt)
        );
        assert_eq!(
            events(&user, mode)[0].text.as_deref(),
            Some("same user message")
        );
        assert_eq!(
            prompt_identity(&user, mode).as_deref(),
            Some(identity.as_str())
        );
        let human = native_user(
            mode,
            &identity,
            "# AGENTS.md instructions are the subject of my question",
        );
        assert_eq!(events(&human, mode)[0].kind, EventKind::UserPrompt);
        assert_eq!(
            events(&human, mode)[0].text.as_deref(),
            human["payload"]
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| human["payload"]["item"]["content"][0]["text"].as_str())
        );
        let opposite = if mode == HistoryMode::Legacy {
            HistoryMode::Paginated
        } else {
            HistoryMode::Legacy
        };
        assert!(
            events(&user, opposite)
                .iter()
                .all(|event| event.kind != EventKind::UserPrompt)
        );
        assert!(prompt_identity(&user, opposite).is_none());
    }
    assert_eq!(
        events(&model_user("retained"), HistoryMode::Model)[0]
            .text
            .as_deref(),
        Some("retained")
    );
}

#[test]
fn header_mode_follows_the_open_replay_source_even_when_the_header_is_outside_the_window() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl");
    std::fs::write(&path, metadata(Some(json!("legacy"))) + "old\n").unwrap();
    let mut source = std::fs::File::open(&path).unwrap();
    let (offset, line, _, _) = crate::rc::tail::window_start(&mut source, 1);
    let handle = same_file::Handle::from_file(source).unwrap();
    let mut tailer = Tailer::at(&path, offset, line, Some(handle), 1);
    let initial = tailer.poll_codex().unwrap();
    assert_eq!(initial.mode, HistoryMode::Legacy);
    assert_eq!(initial.lines[0].text, "old");
    std::fs::rename(&path, directory.path().join("old.jsonl")).unwrap();
    std::fs::write(&path, metadata(Some(json!("paginated"))) + "replacement\n").unwrap();
    let replacement = tailer.poll_codex().unwrap();
    assert_eq!(replacement.mode, HistoryMode::Paginated);
    assert_eq!(replacement.lines[0].text, "replacement");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"append\n")
        .unwrap();
    let appended = tailer.poll_codex().unwrap();
    assert_eq!(appended.mode, HistoryMode::Paginated);
    assert_eq!(appended.lines[0].text, "append");
    std::fs::write(&path, metadata(None)).unwrap();
    assert_eq!(tailer.poll_codex().unwrap().mode, HistoryMode::Legacy);
}

#[test]
fn resumed_tails_and_late_sources_read_their_own_header_before_projecting_new_records() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl");
    let mut late = Tailer::new(&path, true);
    assert_eq!(late.poll_codex().unwrap().mode, HistoryMode::Model);
    std::fs::write(&path, metadata(Some(json!("paginated")))).unwrap();
    assert_eq!(late.poll_codex().unwrap().mode, HistoryMode::Paginated);
    let mut resumed = Tailer::new(&path, false);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"new\n")
        .unwrap();
    let batch = resumed.poll_codex().unwrap();
    assert_eq!(batch.mode, HistoryMode::Paginated);
    assert_eq!(
        batch.lines,
        vec![TailedLine {
            lineno: 1,
            text: "new".into()
        }]
    );
    let replacement = directory.path().join("retargeted.jsonl");
    std::fs::write(&replacement, metadata(None)).unwrap();
    resumed.retarget(&replacement, true);
    assert_eq!(resumed.poll_codex().unwrap().mode, HistoryMode::Legacy);
}

#[test]
fn replacement_before_first_poll_cannot_use_the_prepared_sources_mode() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rollout.jsonl");
    std::fs::write(&path, metadata(Some(json!("legacy"))) + "old\n").unwrap();
    let mut source = std::fs::File::open(&path).unwrap();
    let (offset, line, _, _) = crate::rc::tail::window_start(&mut source, 1);
    let handle = same_file::Handle::from_file(source).unwrap();
    let mut tailer = Tailer::at(&path, offset, line, Some(handle), 1);
    std::fs::rename(&path, directory.path().join("old.jsonl")).unwrap();
    let identity = uuid::Uuid::new_v4().to_string();
    let user = native_user(HistoryMode::Paginated, &identity, "replacement prompt");
    std::fs::write(
        &path,
        metadata(Some(json!("paginated"))) + &format!("{user}\n"),
    )
    .unwrap();
    let batch = tailer.poll_codex().unwrap();
    assert_eq!(batch.mode, HistoryMode::Paginated);
    let (items, _) = crate::rc::supervisor::items_from_lines_with_mode(
        "codex",
        &Redactor::new(Persona::default()),
        &batch.lines,
        batch.mode,
    );
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].native_prompt_id.as_deref(),
        Some(identity.as_str())
    );
    assert_eq!(items[0].event.text.as_deref(), Some("replacement prompt"));
}

#[test]
fn canonical_message_identity_is_independent_of_text_and_rejects_non_uuid_metadata() {
    for mode in [HistoryMode::Legacy, HistoryMode::Paginated] {
        let first = "abcdefab-cdef-4abc-8def-abcdefabcdef".to_string();
        let second = uuid::Uuid::new_v4().to_string();
        let a = native_user(mode, &first, "continue");
        let b = native_user(mode, &second, "continue");
        assert_ne!(prompt_identity(&a, mode), prompt_identity(&b, mode));
        for invalid in [
            "not-a-uuid".to_string(),
            first.to_uppercase(),
            first.replace('-', ""),
        ] {
            let row = native_user(mode, &invalid, "continue");
            assert!(prompt_identity(&row, mode).is_none());
            assert_eq!(events(&row, mode)[0].text.as_deref(), Some("continue"));
        }
    }
}

#[test]
fn native_compaction_does_not_replay_retained_prompts_as_new_speech() {
    let compacted = json!({"type":"compacted","payload":{"replacement_history":[
        {"type":"message","role":"user","content":[{"type":"input_text","text":"retained prompt"}]},
        {"type":"message","role":"assistant","content":[{"type":"output_text","text":"retained answer"}]}
    ]}});
    for mode in [HistoryMode::Legacy, HistoryMode::Paginated] {
        let projected = events(&compacted, mode);
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].kind, EventKind::CompactFiltered);
    }
    let archived = events(&compacted, HistoryMode::Model);
    assert_eq!(archived.len(), 3);
    assert_eq!(archived[1].text.as_deref(), Some("retained prompt"));
    assert_eq!(archived[2].text.as_deref(), Some("retained answer"));
}

#[test]
fn correlation_survives_raw_capping_but_never_bypasses_secret_redaction() {
    let identity = uuid::Uuid::new_v4().to_string();
    for mode in [HistoryMode::Legacy, HistoryMode::Paginated] {
        let raw = native_user(
            mode,
            &identity,
            &"x".repeat(crate::protocol::RAW_LINE_CAP * 2),
        );
        let line = TailedLine {
            lineno: 41,
            text: raw.to_string(),
        };
        let (items, _) = crate::rc::supervisor::items_from_lines_with_mode(
            "codex",
            &Redactor::new(Persona::default()),
            std::slice::from_ref(&line),
            mode,
        );
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].native_prompt_id.as_deref(),
            Some(identity.as_str())
        );
        assert!(items[0].raw_truncated);
        assert_eq!(items[0].event.line, Some(41));
        assert_eq!(
            items[0].object_hash,
            crate::domain::transcript::object_hash(&raw)
        );
        let matcher = crate::domain::secret_filter::Matcher::for_test(&[("identity", &identity)]);
        let redactor = Redactor::with_registered(
            Persona::default(),
            crate::domain::secret_filter::MatcherHandle::new(matcher),
        );
        let (redacted, registered) =
            crate::rc::supervisor::items_from_lines_with_mode("codex", &redactor, &[line], mode);
        assert!(redacted[0].native_prompt_id.is_none());
        assert!(!registered.is_empty());
        assert_ne!(
            redacted[0].object_hash,
            crate::domain::transcript::object_hash(&raw)
        );
        assert!(
            !serde_json::to_string(&redacted[0])
                .unwrap()
                .contains(&identity)
        );
    }
}
