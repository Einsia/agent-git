//! Prepare native transcript objects once, then expose successive immutable snapshot deltas.

use crate::Result;
use crate::adapter::Session;
use crate::domain::{meta, storage, transcript};
use anyhow::{Context as _, ensure};
use std::collections::BTreeMap;

pub(super) struct NativeSnapshots {
    raw_ends: Vec<usize>,
    event_counts: Vec<usize>,
    compact_lines: Vec<usize>,
    ids: Vec<String>,
    pending: BTreeMap<String, Vec<u8>>,
    consumed: usize,
}

impl NativeSnapshots {
    pub(super) fn new(
        raw: &str,
        protected: &str,
        session: &Session,
        source: &str,
        claim: &str,
        end: usize,
    ) -> Result<Self> {
        let mut raw_ends = vec![0];
        let mut event_counts = vec![0];
        let mut log = storage::SnapshotLog::default();
        let mut protected_lines = protected.split_inclusive('\n');
        for raw_line in raw.split_inclusive('\n') {
            if *raw_ends.last().expect("initial boundary") == end {
                break;
            }
            let line = protected_lines
                .next()
                .context("protected transcript lost a source line")?;
            let wrapped = transcript::wrap_lines(line, source, claim);
            let count =
                event_counts.last().expect("initial count") + usize::from(!wrapped.is_empty());
            if !wrapped.is_empty() {
                log.push(&wrapped)?;
            }
            raw_ends.push(raw_ends.last().expect("initial boundary") + raw_line.len());
            event_counts.push(count);
        }
        ensure!(
            raw_ends.last() == Some(&end),
            "snapshot end is not a source line boundary"
        );
        // The complete closed prefix is validated before any objects are written. This also
        // checks cross-turn event collisions and the aggregate materialization limit.
        let (ids, pending) = log.into_parts();
        let mut compact_lines: Vec<_> = session
            .events
            .iter()
            .filter(|event| event.kind.is_compact())
            .filter_map(|event| event.line)
            .collect();
        compact_lines.sort_unstable();
        compact_lines.dedup();
        Ok(Self {
            raw_ends,
            event_counts,
            compact_lines,
            ids,
            pending,
            consumed: 0,
        })
    }

    pub(super) fn files(&mut self, end: usize) -> Result<BTreeMap<String, Vec<u8>>> {
        let line_count = self
            .raw_ends
            .binary_search(&end)
            .map_err(|_| anyhow::anyhow!("snapshot end is not a prepared source boundary"))?;
        let count = self.event_counts[line_count];
        ensure!(
            count >= self.consumed,
            "snapshot prefixes must advance monotonically"
        );
        let compact = self
            .compact_lines
            .partition_point(|line| *line < line_count);
        let view_start = if compact == 0 {
            0
        } else {
            self.event_counts[self.compact_lines[compact - 1]]
        };
        let mut files = BTreeMap::new();
        for id in &self.ids[self.consumed..count] {
            let path = meta::event_path(id)?;
            if let Some(bytes) = self.pending.remove(&path) {
                files.insert(path, bytes);
            }
        }
        files.insert(
            meta::LOG_FILE.into(),
            storage::sequence_text(&self.ids[..count])?.into_bytes(),
        );
        files.insert(
            meta::VIEW_FILE.into(),
            storage::sequence_text(&self.ids[view_start..count])?.into_bytes(),
        );
        self.consumed = count;
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAIM: &str = "agit-0123456789abcdef0123456789abcdef01234567";

    fn compare_prefixes(raw: &str, protected: &str, runtime: &str) {
        let adapter = crate::adapter::get(runtime).unwrap();
        let session = adapter.parse(raw).unwrap();
        let mut prepared =
            NativeSnapshots::new(raw, protected, &session, runtime, CLAIM, raw.len()).unwrap();
        let mut accumulated = BTreeMap::new();
        let mut raw_end = 0;
        let mut protected_end = 0;
        let mut written_events = 0;
        for (raw_line, protected_line) in raw
            .split_inclusive('\n')
            .zip(protected.split_inclusive('\n'))
        {
            raw_end += raw_line.len();
            protected_end += protected_line.len();
            let delta = prepared.files(raw_end).unwrap();
            written_events += delta
                .keys()
                .filter(|path| path.starts_with("events/"))
                .count();
            accumulated.extend(delta);
            let log = transcript::wrap_lines(&protected[..protected_end], runtime, CLAIM);
            let raw_view = transcript::view_of_live(&raw[..raw_end], runtime).unwrap();
            let skipped_lines = raw[..raw_end - raw_view.len()]
                .split_inclusive('\n')
                .count();
            let protected_start: usize = protected
                .split_inclusive('\n')
                .take(skipped_lines)
                .map(str::len)
                .sum();
            let view =
                transcript::wrap_lines(&protected[protected_start..protected_end], runtime, CLAIM);
            assert_eq!(
                accumulated,
                storage::snapshot_files(&log, &view).unwrap(),
                "prefix at {raw_end}"
            );
        }
        assert_eq!(
            written_events,
            accumulated
                .keys()
                .filter(|path| path.starts_with("events/"))
                .count(),
            "an immutable event must be supplied only once across turn snapshots"
        );
    }

    #[test]
    fn native_deltas_match_full_snapshots_at_every_source_boundary() {
        let records = [
            serde_json::json!({"type":"session_meta","payload":{"id":"example"}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"question"}]}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}}),
            serde_json::json!({"type":"compacted","payload":{"window_number":1,"replacement_history":[]}}),
            serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"next"}]}}),
            serde_json::json!({"type":"compacted","payload":{"window_number":2,"replacement_history":[]}}),
        ];
        let mut raw = String::from("\nmalformed\r\n");
        for record in &records {
            raw.push_str(&format!(" {record}\r\n"));
        }
        raw.push_str(&format!("{}\n", records[1]));
        raw.push_str("{\"unfinished\":");
        let protected = raw
            .replace("question", "a differently sized protected value")
            .replace("compacted", "protected-runtime-label");
        compare_prefixes(&raw, &protected, "codex");
        let claude = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"question\"}}\n{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"compactMetadata\":{}}\n{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"answer\"}}\n";
        compare_prefixes(claude, claude, "claude-code");
    }

    #[test]
    fn preparation_stops_at_the_closed_prefix_and_rejects_invalid_coordinates() {
        let closed = "{\"type\":\"session_meta\",\"payload\":{}}\n";
        let raw = format!("{closed}{{\"unfinished\":");
        let session = crate::adapter::get("codex").unwrap().parse(&raw).unwrap();
        let mut prepared =
            NativeSnapshots::new(&raw, closed, &session, "codex", CLAIM, closed.len()).unwrap();
        let snapshot = prepared.files(closed.len()).unwrap();
        let log = transcript::wrap_lines(closed, "codex", CLAIM);
        assert_eq!(snapshot, storage::snapshot_files(&log, &log).unwrap());
        assert!(prepared.files(1).is_err());
        assert!(prepared.files(0).is_err());
        assert!(NativeSnapshots::new(&raw, "", &session, "codex", CLAIM, closed.len()).is_err());
    }
}
