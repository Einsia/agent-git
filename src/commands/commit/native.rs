//! Prepare native transcript objects once, then expose successive immutable snapshot deltas.

use crate::Result;
use crate::adapter::Session;
use crate::domain::{meta, storage, transcript};
use anyhow::{Context as _, ensure};
use sha2::Digest as _;
use std::collections::BTreeMap;

/// Native and projected coordinates share line boundaries, never byte offsets.
pub(super) struct Projection {
    raw: Vec<usize>,
    projected: Vec<usize>,
}

impl Projection {
    pub(super) fn new(raw: &str, projected: &str) -> Result<Self> {
        let ends = |text: &str| {
            let mut out = vec![0];
            for line in text.split_inclusive('\n') {
                out.push(out.last().copied().unwrap_or(0) + line.len());
            }
            out
        };
        let raw = ends(raw);
        let projected = ends(projected);
        ensure!(
            raw.len() == projected.len(),
            "privacy projection changed native line boundaries"
        );
        Ok(Self { raw, projected })
    }

    pub(super) fn end(&self, raw: usize) -> Result<usize> {
        Ok(self.projected[self
            .raw
            .binary_search(&raw)
            .map_err(|_| anyhow::anyhow!("projection end is not a native boundary"))?])
    }
}

/// The settled part of a native transcript as its base commit holds it, taken without reading a
/// settled event: the base LOG's ids, and where the part ends in the transcript. Only a verified
/// native checkpoint lets a settlement use it, because the checkpoint is what proves those
/// transcript bytes unchanged since that commit; the continuity compare proves the same thing by
/// reading every settled event.
pub(super) struct Settled {
    ids: Vec<String>,
    /// The event files the ids name, all present in the base.
    paths: std::collections::BTreeSet<String>,
    /// What the settled event bodies take in all, as the snapshot limits count it.
    bytes: usize,
    /// Where the settled part ends: after the line of the last settled event, as the continuity
    /// compare's boundary falls.
    end: usize,
    /// The number of events before each line of the settled part, and after its last line.
    counts: Vec<usize>,
}

impl Settled {
    /// The settled part of `raw` at `tip`, whose checkpoint ends at `checkpoint`; `None` for any
    /// shape this doesn't expect, which the full read and compare then handles as before.
    pub(super) fn at(
        repo: &crate::domain::repo::Repo,
        tip: &str,
        raw: &str,
        checkpoint: usize,
    ) -> Option<Self> {
        Self::read(repo, tip, raw, checkpoint).ok().flatten()
    }

    fn read(
        repo: &crate::domain::repo::Repo,
        tip: &str,
        raw: &str,
        checkpoint: usize,
    ) -> Result<Option<Self>> {
        let log =
            repo.git_bytes_result(&["cat-file", "blob", &format!("{tip}:{}", meta::LOG_FILE)])?;
        let ids = storage::parse_sequence(std::str::from_utf8(&log)?)?;
        let mut sizes = BTreeMap::new();
        let listing = repo.git(&["ls-tree", "-r", "-l", tip, "--", meta::EVENTS_DIR])?;
        for entry in listing.lines() {
            let (fields, path) = entry
                .split_once('\t')
                .context("ls-tree entry without a path")?;
            let mut fields = fields.split_whitespace();
            let (Some(_), Some("blob"), Some(_), Some(size)) =
                (fields.next(), fields.next(), fields.next(), fields.next())
            else {
                return Ok(None);
            };
            sizes.insert(path.to_owned(), size.parse::<usize>()?);
        }
        let mut paths = std::collections::BTreeSet::new();
        let mut bytes = 0usize;
        for id in &ids {
            let path = meta::event_path(id)?;
            let Some(size) = sizes.get(&path) else {
                return Ok(None);
            };
            bytes = bytes.checked_add(*size).context("LOG size overflow")?;
            paths.insert(path);
        }
        if bytes > storage::MAX_MATERIALIZED_BYTES || ids.len() > storage::MAX_SEQUENCE_EVENTS {
            return Ok(None);
        }
        // One event per line that parses, as wrapping and the continuity compare count them; the
        // part ends after the line of the last settled event.
        let mut counts = vec![0];
        let mut end = 0;
        let mut seen = 0;
        if !ids.is_empty() {
            for line in raw.split_inclusive('\n') {
                seen += usize::from(serde_json::from_str::<serde_json::Value>(line).is_ok());
                end += line.len();
                counts.push(seen);
                if seen == ids.len() {
                    break;
                }
            }
        }
        if seen != ids.len() || end > checkpoint {
            return Ok(None);
        }
        Ok(Some(Self {
            ids,
            paths,
            bytes,
            end,
            counts,
        }))
    }

    /// Where the settled part ends in the transcript.
    pub(super) fn end(&self) -> usize {
        self.end
    }

    /// The event files of the settled part, which the base already holds.
    pub(super) fn paths(&self) -> &std::collections::BTreeSet<String> {
        &self.paths
    }
}

/// The native snapshots of one settlement, turn by turn. Each event body is wrapped again when
/// its turn is written, from the protected line it came from, and must hash to what validation
/// saw: holding every wrapped body from validation to its turn would keep a long conversation's
/// whole history in memory once more.
pub(super) struct NativeSnapshots {
    raw_ends: Vec<usize>,
    event_counts: Vec<usize>,
    compact_lines: Vec<usize>,
    /// The transcript lines before `raw_ends[0]`, which a settled part holds.
    first_line: usize,
    /// Where the view starts when its last compaction lies in the settled part.
    settled_view_start: usize,
    ids: Vec<String>,
    /// How many of `ids` a settled part holds; `lines` starts after them.
    settled: usize,
    /// Where each event's protected line sits in the protected transcript.
    lines: Vec<std::ops::Range<usize>>,
    /// The digest of each event body not yet written, by its path.
    pending: BTreeMap<String, [u8; 32]>,
    source: String,
    claim: String,
    consumed: usize,
}

impl NativeSnapshots {
    /// With `settled`, the snapshots go on from that settled part, and `protected` starts where
    /// it ends; without it, `protected` is the whole protected transcript.
    pub(super) fn new(
        raw: &str,
        protected: &str,
        session: &Session,
        source: &str,
        claim: &str,
        end: usize,
        settled: Option<&Settled>,
    ) -> Result<Self> {
        let (start, first_line, mut log) = match settled {
            Some(settled) => (
                settled.end,
                settled.counts.len() - 1,
                storage::SnapshotLog::<[u8; 32]>::after(settled.ids.clone(), settled.bytes)?,
            ),
            None => (0, 0, storage::SnapshotLog::<[u8; 32]>::default()),
        };
        let settled_count = settled.map_or(0, |settled| settled.ids.len());
        let mut raw_ends = vec![start];
        let mut event_counts = vec![settled_count];
        let mut lines = Vec::new();
        let mut protected_lines = protected.split_inclusive('\n');
        let mut at = 0;
        for raw_line in raw[start..].split_inclusive('\n') {
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
                lines.push(at..at + line.len());
            }
            at += line.len();
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
        let settled_view_start = settled.map_or(0, |settled| {
            compact_lines
                .iter()
                .rev()
                .find(|line| **line < first_line)
                .map_or(0, |line| settled.counts[*line])
        });
        Ok(Self {
            raw_ends,
            event_counts,
            compact_lines,
            first_line,
            settled_view_start,
            ids,
            settled: settled_count,
            lines,
            pending,
            source: source.to_owned(),
            claim: claim.to_owned(),
            consumed: settled_count,
        })
    }

    /// The event files of the snapshot that ends at `end` that `base` already holds, which that
    /// snapshot then neither wraps nor writes again: `base` lists the paths of a v1 base tree, and
    /// only events the snapshot's own LOG lists are taken from it. An event file is named by the
    /// hash of its bytes, so the one in `base` is the file that would be written. Call it before
    /// that snapshot's [`Self::files`].
    pub(super) fn reuse(
        &mut self,
        end: usize,
        base: &std::collections::HashSet<String>,
    ) -> Result<std::collections::BTreeSet<String>> {
        let line_count = self
            .raw_ends
            .binary_search(&end)
            .map_err(|_| anyhow::anyhow!("snapshot end is not a prepared source boundary"))?;
        let count = self.event_counts[line_count];
        let mut kept = std::collections::BTreeSet::new();
        for id in &self.ids[self.consumed..count] {
            let path = meta::event_path(id)?;
            if base.contains(&path) && self.pending.remove(&path).is_some() {
                kept.insert(path);
            }
        }
        Ok(kept)
    }

    /// The files of the snapshot that ends at `end`, from `protected`, the same protected
    /// transcript these snapshots were prepared from.
    pub(super) fn files(
        &mut self,
        protected: &str,
        end: usize,
    ) -> Result<BTreeMap<String, Vec<u8>>> {
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
            .partition_point(|line| *line < self.first_line + line_count);
        let view_start = match compact.checked_sub(1).map(|at| self.compact_lines[at]) {
            None => 0,
            Some(line) if line >= self.first_line => self.event_counts[line - self.first_line],
            Some(_) => self.settled_view_start,
        };
        let mut files = BTreeMap::new();
        for (id, line) in self.ids[self.consumed..count]
            .iter()
            .zip(&self.lines[self.consumed - self.settled..count - self.settled])
        {
            let path = meta::event_path(id)?;
            if let Some(digest) = self.pending.remove(&path) {
                let line = protected
                    .get(line.clone())
                    .context("protected transcript changed under its snapshots")?;
                let body = transcript::wrap_lines(line, &self.source, &self.claim).into_bytes();
                ensure!(
                    <[u8; 32]>::from(sha2::Sha256::digest(&body)) == digest,
                    "event {id} changed between validation and writing"
                );
                files.insert(path, body);
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
            NativeSnapshots::new(raw, protected, &session, runtime, CLAIM, raw.len(), None)
                .unwrap();
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
            let delta = prepared.files(protected, raw_end).unwrap();
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

    /// Turn by turn, the snapshots write exactly the files a whole snapshot of the same prefix
    /// holds, each event once, though every event body is wrapped again from its protected line
    /// when its turn is written rather than kept from validation. A body wrapped differently on
    /// the way out (another line, runtime or claim) fails its digest or this comparison.
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
            NativeSnapshots::new(&raw, closed, &session, "codex", CLAIM, closed.len(), None)
                .unwrap();
        let snapshot = prepared.files(closed, closed.len()).unwrap();
        let log = transcript::wrap_lines(closed, "codex", CLAIM);
        assert_eq!(snapshot, storage::snapshot_files(&log, &log).unwrap());
        assert!(prepared.files(closed, 1).is_err());
        assert!(prepared.files(closed, 0).is_err());
        assert!(
            NativeSnapshots::new(&raw, "", &session, "codex", CLAIM, closed.len(), None).is_err()
        );
    }
}
