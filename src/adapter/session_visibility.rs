//! Native metadata narrows user-facing discovery without changing explicit session lookup.

use std::io::{BufRead, BufReader, Read};
use std::path::Path;

const METADATA_BYTES: u64 = 1024 * 1024;
const METADATA_RECORDS: usize = 64;

/// The first authoritative origin owns the file; later records cannot reclassify it.
/// Missing legacy metadata is not evidence of an internal conversation. Inspection stays
/// bounded even when a transcript starts with a large message or filesystem snapshot.
pub(super) fn has_internal_metadata(
    path: &Path,
    classify: impl Fn(&serde_json::Value) -> Option<bool>,
) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    BufReader::new(file.take(METADATA_BYTES))
        .lines()
        .take(METADATA_RECORDS)
        .filter_map(Result::ok)
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(&line).ok())
        .find_map(|record| classify(&record))
        .unwrap_or(false)
}

pub(super) fn internal_codex_source(
    thread_source: Option<&str>,
    source: Option<&serde_json::Value>,
) -> bool {
    let internal = |value: &str| matches!(value, "subagent" | "guardian_review");
    thread_source.is_some_and(internal)
        || source.is_some_and(|value| {
            value.as_str().is_some_and(internal)
                || value
                    .as_object()
                    .is_some_and(|object| object.contains_key("subagent"))
        })
}

pub(super) fn internal_codex_file(path: &Path) -> bool {
    has_internal_metadata(path, |record| {
        (record["type"] == "session_meta").then(|| {
            internal_codex_source(
                record["payload"]["thread_source"].as_str(),
                record["payload"].get("source"),
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_origin_is_decided_before_reading_conversation_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rollout.jsonl");
        for (origin, internal) in [("user", false), ("subagent", true)] {
            std::fs::write(&path, format!("{{\"type\":\"session_meta\",\"payload\":{{\"thread_source\":\"{origin}\"}}}}\n{{\"type\":\"session_meta\",\"payload\":{{\"thread_source\":\"guardian_review\"}}}}\n")).unwrap();
            assert_eq!(internal_codex_file(&path), internal);
        }
        std::fs::write(&path, "{\"type\":\"session_meta\",\"payload\":{}}\n").unwrap();
        assert!(!internal_codex_file(&path), "legacy origin remains visible");
    }
}
