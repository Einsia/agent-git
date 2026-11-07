use super::paired;
use crate::domain::{storage, transcript::Envelope};

fn record(source: &str, content: serde_json::Value) -> (String, Envelope) {
    let envelope = Envelope {
        source: source.into(),
        session_id: format!("agit-{}", "a".repeat(40)),
        object_hash: crate::domain::transcript::object_hash(&content),
        content,
    };
    (
        storage::canonical_event_id(&storage::envelope_line(&envelope)),
        envelope,
    )
}

fn claude(uuid: &str, kind: &str, text: &str) -> (String, Envelope) {
    record(
        "claude-code",
        serde_json::json!({"type": kind, "uuid": uuid, "message": {"role": kind, "content": text}}),
    )
}

/// A published event stands for the kept record in its place only when the pair checks: the same
/// event, or a record protection changed but whose runtime still names it the same. Equal counts
/// whose records don't pair, as after a rule that keeps one record more and another less, map
/// nothing.
#[test]
fn a_published_event_pairs_only_with_the_record_it_stands_for() {
    let first = claude("u1", "user", "Rename the flag.");
    let answer = claude("u2", "assistant", "Renamed.");
    let next = claude("u3", "user", "Next.");
    let protected = claude("u2", "assistant", "Renamed [protected].");
    let pairs = paired(
        &[first.clone(), answer.clone(), next.clone()],
        &[first.clone(), protected.clone(), next.clone()],
    )
    .unwrap();
    assert_eq!(
        pairs,
        vec![
            (first.0.clone(), first.0.clone()),
            (protected.0.clone(), answer.0.clone()),
            (next.0.clone(), next.0.clone()),
        ]
    );
    assert_eq!(
        paired(
            &[first.clone(), answer.clone()],
            &[first.clone(), next.clone()]
        ),
        None
    );
    assert_eq!(paired(&[first.clone()], &[first.clone(), next]), None);
    // A runtime that names no record apart from its text pairs only the same event.
    let said = record(
        "codex",
        serde_json::json!({"type": "message", "text": "Renamed."}),
    );
    let changed = record(
        "codex",
        serde_json::json!({"type": "message", "text": "[protected]"}),
    );
    assert_eq!(paired(&[said], &[changed]), None);
}
