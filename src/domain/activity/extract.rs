//! Newly appended user and assistant text of one contribution, read in bounded record windows.

use super::Assistant;
use super::history::{Blob, Candidate, Store};
use crate::adapter::EventKind;
use crate::domain::transcript::Envelope;
use crate::domain::{meta, storage, transcript};
use anyhow::{Context, Result, anyhow, ensure};
use std::collections::HashMap;
use std::rc::Rc;

pub(super) const MAX_META: usize = 64 * 1024;
const MAX_WINDOW_BYTES: usize = 8 * 1024 * 1024;
const MAX_WINDOW_RECORDS: usize = 1024;
/// Windows smaller than this are validated on the calling thread; more threads would only add
/// startup cost.
const PARALLEL_BYTES: usize = 1024 * 1024;
const MAX_VALIDATORS: usize = 8;
/// One LOG line: an event ID and its LF.
const ID_LINE: usize = meta::EVENT_ID_HEX_LEN + 1;
/// Every sequence the storage format admits is readable.
const MAX_SEQUENCE_BYTES: usize = storage::MAX_SEQUENCE_EVENTS * ID_LINE;
/// Transcripts a parser needs whole: legacy LOGs and OpenCode.
const MAX_CONTEXT_BYTES: usize = 32 * 1024 * 1024;
/// Cached sequences beyond the newest are evicted past this size.
const SEQUENCE_CACHE_BYTES: usize = 96 * 1024 * 1024;
/// Encrypted publications replace excluded content with this reserved line.
const PRIVACY_PLACEHOLDER: &str = "[Content omitted by privacy policy]";

/// The page's read budget cannot cover the next read; the page ends and a continuation retries
/// the same deterministic window with a fresh budget.
#[derive(Debug, thiserror::Error)]
#[error("activity read budget exhausted")]
pub(super) struct Exhausted;

#[derive(Clone)]
pub(super) struct Meta {
    pub session: String,
    pub turn: Option<u32>,
    pub contribution: bool,
    pub account_id: Option<String>,
    runtime: String,
    file_line: bool,
    legacy: bool,
}

pub(super) fn parse_meta(raw: &[u8], commit: &str) -> Result<Meta> {
    let metadata = meta::parse_strict(std::str::from_utf8(raw)?, commit)?;
    Ok(Meta {
        file_line: metadata.is_file_line(),
        legacy: matches!(metadata.layout, meta::LayoutVersion::V0),
        contribution: matches!(metadata.kind, meta::Kind::Turn | meta::Kind::Archive),
        account_id: metadata.committer_account.map(|account| account.account_id),
        session: metadata.session,
        turn: metadata.turn,
        runtime: metadata.runtime,
    })
}

enum Metadata {
    Absent,
    Invalid,
    Present(Meta),
}

/// Event IDs kept in LOG wire form, so a long session costs its LOG size rather than one
/// allocation per record, and a parent prefix is a byte comparison.
struct Sequence {
    log: String,
    /// A legacy LOG holds the canonical records themselves.
    legacy: Option<String>,
}

impl Sequence {
    fn new(log: String, legacy: Option<String>) -> Result<Self> {
        ensure!(
            log.len().is_multiple_of(ID_LINE)
                && log.as_bytes().chunks(ID_LINE).all(|line| {
                    line[ID_LINE - 1] == b'\n'
                        && std::str::from_utf8(&line[..ID_LINE - 1]).is_ok_and(meta::is_event_id)
                }),
            "saved LOG contains an invalid event ID"
        );
        Ok(Self { log, legacy })
    }

    fn len(&self) -> usize {
        self.log.len() / ID_LINE
    }

    fn ids(&self, from: usize, to: usize) -> impl Iterator<Item = &str> {
        self.log[from * ID_LINE..to * ID_LINE].split_terminator('\n')
    }

    fn bytes(&self) -> usize {
        self.log.len() + self.legacy.as_ref().map_or(0, String::len)
    }
}

pub(super) struct Text {
    pub role: &'static str,
    /// One-based source record.
    pub line: usize,
    pub text: String,
}

pub(super) struct Window {
    pub texts: Vec<Text>,
    pub next_record: usize,
    pub records: usize,
}

/// A window read ahead to decide whether an assistant message is its turn's last.
struct Peeked {
    commit: String,
    record: usize,
    window: Window,
}

pub(super) struct Extractor<'s, S> {
    store: &'s mut S,
    remaining: usize,
    metas: HashMap<String, Metadata>,
    /// Ascending order reads a parent just before its child, so recent sequences serve both.
    sequences: Vec<(String, Rc<Sequence>)>,
    peeked: Option<Peeked>,
}

impl<'s, S: Store> Extractor<'s, S> {
    pub(super) fn new(store: &'s mut S, budget: usize) -> Self {
        Self {
            store,
            remaining: budget,
            metas: HashMap::new(),
            sequences: Vec::new(),
            peeked: None,
        }
    }

    fn charge(&mut self, bytes: usize) -> Result<()> {
        if bytes > self.remaining {
            return Err(Exhausted.into());
        }
        self.remaining -= bytes;
        Ok(())
    }

    /// Batch-read session metadata for commits not read yet.
    pub(super) fn metas(&mut self, commits: &[&str]) -> Result<()> {
        let mut unread: Vec<&str> = commits
            .iter()
            .copied()
            .filter(|commit| !self.metas.contains_key(*commit))
            .collect();
        unread.sort_unstable();
        unread.dedup();
        if unread.is_empty() {
            return Ok(());
        }
        let objects: Vec<_> = unread.iter().map(|commit| (*commit, meta::FILE)).collect();
        let blobs = self.store.read(&objects, MAX_META)?;
        ensure!(
            blobs.len() == unread.len(),
            "activity metadata reply is incomplete"
        );
        let bytes = blobs
            .iter()
            .map(|blob| match blob {
                Blob::Body(raw) => raw.len(),
                _ => 0,
            })
            .sum();
        self.charge(bytes)?;
        for (commit, blob) in unread.into_iter().zip(blobs) {
            let state = match blob {
                Blob::Missing => Metadata::Absent,
                Blob::Large => Metadata::Invalid,
                Blob::Body(raw) => {
                    parse_meta(&raw, commit).map_or(Metadata::Invalid, Metadata::Present)
                }
            };
            self.metas.insert(commit.into(), state);
        }
        Ok(())
    }

    pub(super) fn has_meta(&self, commit: &str) -> bool {
        self.metas.contains_key(commit)
    }

    pub(super) fn meta(&mut self, commit: &str) -> Result<Option<Meta>> {
        self.metas(&[commit])?;
        match &self.metas[commit] {
            Metadata::Absent => Ok(None),
            Metadata::Invalid => anyhow::bail!("invalid session metadata at {commit}"),
            Metadata::Present(metadata) => Ok(Some(metadata.clone())),
        }
    }

    fn sequence(&mut self, commit: &str, metadata: &Meta) -> Result<Rc<Sequence>> {
        if let Some(index) = self.sequences.iter().position(|(c, _)| c == commit) {
            let hit = self.sequences.remove(index);
            let sequence = hit.1.clone();
            self.sequences.push(hit);
            return Ok(sequence);
        }
        let (path, maximum) = if metadata.legacy {
            (meta::LEGACY_LOG_FILE, MAX_CONTEXT_BYTES)
        } else {
            (meta::LOG_FILE, MAX_SEQUENCE_BYTES)
        };
        let raw = match self.store.read(&[(commit, path)], maximum)?.pop() {
            Some(Blob::Body(raw)) => raw,
            Some(Blob::Large) => anyhow::bail!("activity LOG exceeds the read budget"),
            _ => anyhow::bail!("missing LOG"),
        };
        self.charge(raw.len())?;
        let text = String::from_utf8(raw).context("saved LOG is not UTF-8")?;
        let sequence = Rc::new(if metadata.legacy {
            let canonical = storage::canonical_v0(&text)?;
            let mut log = String::with_capacity(canonical.lines().count() * ID_LINE);
            for line in canonical.split_inclusive('\n') {
                log.push_str(&storage::canonical_event_id(line));
                log.push('\n');
            }
            Sequence::new(log, Some(canonical))?
        } else {
            Sequence::new(text, None)?
        });
        self.sequences.push((commit.into(), sequence.clone()));
        let mut cached: usize = self.sequences.iter().map(|(_, s)| s.bytes()).sum();
        while cached > SEQUENCE_CACHE_BYTES && self.sequences.len() > 1 {
            cached -= self.sequences.remove(0).1.bytes();
        }
        Ok(sequence)
    }

    /// Records appended since the first parent, from `record` on (zero starts after the parent).
    /// Windows and their filtering depend only on immutable content, so a continuation sees the
    /// same messages at the same indexes.
    pub(super) fn window(
        &mut self,
        candidate: &Candidate,
        record: usize,
        assistant: Assistant,
    ) -> Result<Window> {
        let peeked = self
            .peeked
            .as_ref()
            .is_some_and(|p| p.commit == candidate.key.commit && p.record == record);
        let mut window = match if peeked { self.peeked.take() } else { None } {
            Some(peeked) => peeked.window,
            None => self.read_window(candidate, record)?,
        };
        if assistant == Assistant::Last {
            // The last message may continue in a later window; it stays only if no assistant
            // message follows it before the next user message.
            let mut followed = false;
            if window.texts.last().is_some_and(|t| t.role == "assistant") {
                let mut next = window.next_record;
                while next < window.records {
                    let ahead = self.read_window(candidate, next)?;
                    let role = ahead.texts.first().map(|t| t.role);
                    let after = ahead.next_record;
                    let records = ahead.records;
                    self.peeked = Some(Peeked {
                        commit: candidate.key.commit.clone(),
                        record: next,
                        window: ahead,
                    });
                    if let Some(role) = role {
                        followed = role == "assistant";
                        break;
                    }
                    if after == records {
                        break;
                    }
                    next = after;
                }
            }
            let texts = std::mem::take(&mut window.texts);
            let mut texts = texts.into_iter().peekable();
            while let Some(text) = texts.next() {
                let last = match texts.peek() {
                    Some(next) => next.role == "user",
                    None => !followed,
                };
                if text.role == "user" || last {
                    window.texts.push(text);
                }
            }
        }
        Ok(window)
    }

    fn read_window(&mut self, candidate: &Candidate, record: usize) -> Result<Window> {
        let commit = candidate.key.commit.as_str();
        let current = self.sequence(commit, &candidate.meta)?;
        let previous = match &candidate.parent {
            Some(parent) => match self.meta(parent)? {
                Some(metadata)
                    if metadata.session == candidate.meta.session && !metadata.file_line =>
                {
                    let previous = self.sequence(parent, &metadata)?;
                    ensure!(
                        current.log.starts_with(&previous.log),
                        "activity requires append-only LOG history"
                    );
                    previous.len()
                }
                _ => 0,
            },
            None => 0,
        };
        let records = current.len();
        let start = if record == 0 { previous } else { record };
        ensure!(
            start >= previous && start <= records,
            "invalid activity record cursor"
        );
        if start == records {
            return Ok(Window {
                texts: Vec::new(),
                next_record: records,
                records,
            });
        }
        let read;
        let (text, offset, next_record) = match &current.legacy {
            Some(canonical) => (canonical.as_str(), 0, records),
            None => {
                // OpenCode correlates parts across records, so its parser needs every record.
                let windowed = candidate.meta.runtime != "opencode";
                let offset = if windowed { start } else { 0 };
                let end = if windowed {
                    records.min(offset + MAX_WINDOW_RECORDS)
                } else {
                    records
                };
                let ids: Vec<&str> = current.ids(offset, end).collect();
                let count;
                (read, count) = self.events(commit, &ids, windowed)?;
                (read.as_str(), offset, offset + count)
            }
        };
        // Legacy records come from the validating canonicalizer and need no content IDs.
        let ids = current
            .legacy
            .is_none()
            .then(|| current.ids(offset, next_record).collect::<Vec<_>>());
        let envelopes = authenticate(text, ids.as_deref())?;
        let session = transcript::display::parse_validated(&envelopes)?;
        let mut texts = Vec::new();
        for event in session.events {
            let role = match event.kind {
                EventKind::UserPrompt | EventKind::UserInterjection => "user",
                EventKind::AssistantReply => "assistant",
                _ => continue,
            };
            let line = event.line.context("missing source coordinate")?;
            let envelope = envelopes.get(line).context("invalid source coordinate")?;
            let absolute = offset + line;
            let synthetic = envelope.content["agit"] == "merge_summary"
                || (envelope.source == "codex" && envelope.content["type"] == "compacted");
            if absolute < start
                || absolute >= next_record
                || synthetic
                || envelope.session_id != candidate.meta.session
            {
                continue;
            }
            let Some(mut text) = event.text else {
                continue;
            };
            if candidate.published {
                text = text
                    .split_inclusive('\n')
                    .filter(|line| line.trim_end_matches(['\r', '\n']) != PRIVACY_PLACEHOLDER)
                    .collect();
            }
            if !text.is_empty() {
                texts.push(Text {
                    role,
                    line: absolute + 1,
                    text,
                });
            }
        }
        Ok(Window {
            texts,
            next_record,
            records,
        })
    }

    /// The leading records of `ids` within the window bound, or all of them when unwindowed.
    fn events(&mut self, commit: &str, ids: &[&str], windowed: bool) -> Result<(String, usize)> {
        let paths = ids
            .iter()
            .map(|id| meta::event_path(id))
            .collect::<Result<Vec<_>>>()?;
        let objects: Vec<_> = paths.iter().map(|path| (commit, path.as_str())).collect();
        let located = self
            .store
            .locate(&objects)?
            .into_iter()
            .map(|object| object.context("activity event is unavailable"))
            .collect::<Result<Vec<(String, u64)>>>()?;
        ensure!(
            located.len() == ids.len(),
            "activity event reply is incomplete"
        );
        let sizes: Vec<u64> = located.iter().map(|(_, size)| *size).collect();
        let count = if windowed {
            window_count(&sizes)?
        } else {
            ensure!(
                sizes.iter().sum::<u64>() <= MAX_CONTEXT_BYTES as u64,
                "activity transcript exceeds the read budget"
            );
            sizes.len()
        };
        let bytes = usize::try_from(sizes[..count].iter().sum::<u64>())?;
        self.charge(bytes)?;
        let oids: Vec<&str> = located[..count]
            .iter()
            .map(|(oid, _)| oid.as_str())
            .collect();
        let blobs = self.store.blobs(&oids, storage::MAX_EVENT_BYTES)?;
        ensure!(blobs.len() == count, "activity event reply is incomplete");
        let mut text = String::with_capacity(bytes);
        for blob in blobs {
            let Blob::Body(raw) = blob else {
                anyhow::bail!("activity event is unavailable");
            };
            // The window's content-ID check rejects a blob that is not exactly one line.
            text.push_str(std::str::from_utf8(&raw)?);
        }
        Ok((text, count))
    }
}

/// Check each record against its content ID, when given, and parse it. A matching ID
/// authenticates the bytes the writer validated, so its canonical-form and object-hash checks
/// need not run again.
///
/// Lines are split across threads by bytes, not by count: one tool-output record can outweigh
/// the rest of its window, and equal line counts would leave one thread with most of the work.
fn authenticate(text: &str, ids: Option<&[&str]>) -> Result<Vec<Envelope>> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if let Some(ids) = ids {
        ensure!(
            ids.len() == lines.len(),
            "activity event differs from its content ID"
        );
    }
    let check = |first: usize, lines: &[&str]| -> Result<Vec<Envelope>> {
        let mut envelopes = Vec::with_capacity(lines.len());
        for (index, line) in lines.iter().enumerate() {
            if let Some(ids) = ids {
                ensure!(
                    storage::canonical_event_id(line) == ids[first + index],
                    "activity event differs from its content ID"
                );
            }
            envelopes.push(storage::parse_authenticated_envelope(line)?);
        }
        Ok(envelopes)
    };
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(MAX_VALIDATORS);
    if text.len() < PARALLEL_BYTES || threads == 1 {
        return check(0, &lines);
    }
    let share = text.len().div_ceil(threads);
    let mut parts = Vec::new();
    let (mut first, mut bytes) = (0, 0);
    for (index, line) in lines.iter().enumerate() {
        bytes += line.len();
        if bytes >= share || index + 1 == lines.len() {
            parts.push(first..index + 1);
            (first, bytes) = (index + 1, 0);
        }
    }
    std::thread::scope(|scope| {
        let handles: Vec<_> = parts
            .into_iter()
            .map(|part| {
                let (lines, check) = (&lines, &check);
                scope.spawn(move || check(part.start, &lines[part]))
            })
            .collect();
        let mut envelopes = Vec::with_capacity(lines.len());
        for handle in handles {
            envelopes.extend(
                handle
                    .join()
                    .map_err(|_| anyhow!("activity validation panicked"))??,
            );
        }
        Ok(envelopes)
    })
}

/// A window stops between immutable records; a single larger record uses the event ceiling.
fn window_count(sizes: &[u64]) -> Result<usize> {
    let mut bytes = 0u64;
    let mut count = 0;
    for &size in sizes {
        if count > 0 && size > (MAX_WINDOW_BYTES as u64).saturating_sub(bytes) {
            break;
        }
        ensure!(
            size <= storage::MAX_EVENT_BYTES as u64,
            "activity event exceeds the read budget"
        );
        bytes += size;
        count += 1;
    }
    Ok(count)
}
