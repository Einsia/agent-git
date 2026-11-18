//! Bounded source replay carries real records across a verified settlement cut.

use crate::adapter::{EventKind, Session};
use crate::domain::link::{
    Link,
    continuation::{CONTEXT_BYTES, Carrier, Record, State, VERSION},
};
use crate::domain::{meta, repo::Repo};
use sha2::{Digest as _, Sha256};

pub(super) struct Prepared {
    pub bytes: Vec<u8>,
    pub state: State,
    pub context_len: usize,
    pub header: Option<serde_json::Value>,
    pub titles: Vec<String>,
    pub carriers: Vec<Carrier>,
    pub settled: super::native::Settled,
    prefix: Sha256,
    facts: Facts,
}

fn directory(repo: &Repo) -> Option<std::path::PathBuf> {
    repo.common_dir_with_policy(crate::domain::repo::ReadPolicy::LocalOnly)
        .ok()?
        .canonicalize()
        .ok()
}

fn oid(repo: &Repo, tip: &str, path: &str) -> Option<String> {
    let oid = repo.git(&["rev-parse", &format!("{tip}:{path}")]).ok()?;
    let oid = oid.trim();
    (oid.len() == 40 && oid.bytes().all(|b| b.is_ascii_hexdigit())).then(|| oid.to_owned())
}

pub(super) fn carriers(link: &Link) -> Option<Vec<Carrier>> {
    if link.native_binding.is_some() {
        #[cfg(feature = "rc")]
        {
            let spans = link.capture_spans().ok()?;
            let first = spans.first()?;
            let path = first.path.clone();
            let mut end = 0u64;
            // A paginated source also depends on parent-header ordinals outside its
            // logical bytes. Such ancestry requires the complete lineage validator.
            for span in spans {
                if span.path != path || span.file.start != end || span.stream_start != end {
                    return None;
                }
                end = span.file.end;
            }
            return Some(vec![Carrier {
                path,
                start: 0,
                end,
            }]);
        }
        #[cfg(not(feature = "rc"))]
        return None;
    }
    let path = link.resolve()?;
    if crate::adapter::codex_rollout::is_compressed(&path) {
        return None;
    }
    let end = std::fs::metadata(&path).ok()?.len();
    Some(vec![Carrier {
        path,
        start: 0,
        end,
    }])
}

fn cut(carriers: &[Carrier], mut bytes: u64) -> Option<Vec<Carrier>> {
    let mut out = Vec::new();
    for carrier in carriers {
        if bytes == 0 {
            break;
        }
        let length = bytes.min(carrier.end.checked_sub(carrier.start)?);
        out.push(Carrier {
            path: carrier.path.clone(),
            start: carrier.start,
            end: carrier.start.checked_add(length)?,
        });
        bytes -= length;
    }
    (bytes == 0).then_some(out)
}

fn parse(
    runtime: &str,
    text: &str,
    header: Option<&serde_json::Value>,
    seen: bool,
) -> crate::Result<Session> {
    if runtime == "codex" {
        let records = text.lines().enumerate().filter_map(|(line, text)| {
            serde_json::from_str::<serde_json::Value>(text.trim())
                .ok()
                .map(|record| (line, record))
        });
        Ok(crate::adapter::codex::parse_records_after(
            records, header, seen,
        ))
    } else {
        crate::adapter::get(runtime)?.parse(text)
    }
}

fn unsupported(value: &serde_json::Value) -> bool {
    value.pointer("/attachment/type").and_then(|v| v.as_str()) == Some("hook_additional_context")
        || (value["type"] == "event_msg" && value["payload"]["type"] == "user_message")
        || value
            .pointer("/payload/turn_id")
            .and_then(|v| v.as_str())
            .is_some_and(|id| id.starts_with("external-import-turn-"))
}

#[derive(Clone, Copy, Default)]
struct Position {
    bytes: u64,
    events: usize,
    prompts: u32,
    seen: bool,
}

/// Derive cache semantics from verified records. The scratch record is admitted
/// before collection; its contents are discarded after projecting that record.
#[derive(Clone)]
struct Facts {
    runtime: String,
    position: Position,
    close: Position,
    origin: Option<Position>,
    header: Option<(Record, serde_json::Value)>,
    titles: [Option<Record>; 3],
    max_timestamp: Option<i64>,
    view_start: usize,
    modern: bool,
    pending: Vec<u8>,
}

impl Facts {
    fn new(runtime: &str) -> Self {
        Self {
            runtime: runtime.to_owned(),
            position: Position::default(),
            close: Position::default(),
            origin: None,
            header: None,
            titles: [None, None, None],
            max_timestamp: None,
            view_start: 0,
            modern: false,
            pending: Vec::new(),
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> Option<()> {
        for part in bytes.split_inclusive(|byte| *byte == b'\n') {
            if part.len() > CONTEXT_BYTES.checked_sub(self.pending.len())? {
                return None;
            }
            self.pending.try_reserve_exact(part.len()).ok()?;
            self.pending.extend_from_slice(part);
            if part.ends_with(b"\n") {
                let record = std::mem::take(&mut self.pending);
                self.record(&record)?;
            }
        }
        Some(())
    }

    fn record(&mut self, bytes: &[u8]) -> Option<()> {
        let text = std::str::from_utf8(bytes).ok()?;
        let before = self.position;
        self.position.bytes = before.bytes.checked_add(bytes.len() as u64)?;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return Some(());
        };
        if unsupported(&value) {
            return None;
        }
        if self.runtime == "codex" && value["type"] == "session_meta" {
            // A noninitial or repeated header needs the complete parser's identity rules.
            if before.bytes != 0 || bytes.len() > crate::domain::link::continuation::STATE_BYTES {
                return None;
            }
            // Pagination eligibility comes from the held source observation, so a
            // different path observation cannot authorize replay of this header.
            if value
                .pointer("/payload/history_mode")
                .and_then(|v| v.as_str())
                == Some("paginated")
                && value.pointer("/payload/history_base").is_some()
            {
                return None;
            }
            self.header = Some((
                Record {
                    start: 0,
                    end: self.position.bytes,
                },
                value.clone(),
            ));
        }
        let parsed = if self.runtime == "codex" {
            crate::adapter::codex::parse_records_after(
                std::iter::once((0, &value)),
                self.header.as_ref().map(|(_, v)| v),
                before.seen,
            )
        } else {
            crate::adapter::get(&self.runtime).ok()?.parse(text).ok()?
        };
        let prompts = u32::try_from(
            parsed
                .events
                .iter()
                .filter(|e| e.kind == EventKind::UserPrompt)
                .count(),
        )
        .ok()?;
        if prompts > 0 {
            self.origin = Some(if self.runtime == "codex" {
                self.close
            } else {
                before
            });
        }
        self.position.prompts = before.prompts.checked_add(prompts)?;
        if parsed.events.iter().any(|e| e.kind.is_compact()) {
            self.view_start = before.events;
        }
        self.position.events = before.events.checked_add(1)?;
        self.max_timestamp = parsed
            .events
            .iter()
            .filter_map(|e| e.timestamp.as_deref())
            .filter_map(super::native_time)
            .chain(self.max_timestamp)
            .max();
        self.modern |= super::records_completions(&self.runtime, text, &parsed);
        if self.runtime == "codex" {
            self.position.seen |= (value["type"] == "response_item"
                && matches!(
                    value["payload"]["type"].as_str(),
                    Some("message" | "agent_message")
                ))
                || (value["type"] == "compacted"
                    && value["payload"]["replacement_history"]
                        .as_array()
                        .is_some_and(|items| !items.is_empty()));
            if value["type"] == "event_msg"
                && matches!(
                    value["payload"]["type"].as_str(),
                    Some("task_complete" | "turn_aborted")
                )
            {
                self.close = self.position;
            }
        }
        if self.runtime == "claude-code"
            && ["\"custom-title\"", "\"ai-title\"", "\"summary\""]
                .iter()
                .any(|kind| text.contains(kind))
            && let Some(index) = title_index(&value)
        {
            self.titles[index] = Some(Record {
                start: before.bytes,
                end: self.position.bytes,
            });
        }
        Some(())
    }

    fn header_range(&self) -> Option<Record> {
        (self.runtime == "codex" && self.origin?.bytes > 0)
            .then(|| self.header.as_ref().map(|(range, _)| range.clone()))
            .flatten()
    }

    fn title_ranges(&self) -> Vec<Record> {
        let mut ranges: Vec<_> = self.titles.iter().flatten().cloned().collect();
        ranges.sort_by_key(|range| range.start);
        ranges
    }

    fn validates(&self, state: &State, turn: u32) -> bool {
        let Some(origin) = self.origin else {
            return false;
        };
        self.pending.is_empty()
            && self.modern
            && self.position.bytes == state.bytes
            && self.position.prompts == turn
            && origin.bytes == state.origin
            && origin.events == state.events_before
            && origin.prompts == state.prompts_before
            && origin.seen == state.seen_message
            && self.max_timestamp == state.max_timestamp
            && self.view_start == state.view_start
            && self.header_range() == state.header
            && self.title_ranges() == state.titles
            && (self.runtime != "codex" || origin.bytes == 0 || self.header.is_some())
    }
}

fn projection(session: &Session, lines: usize) -> Option<String> {
    struct Writer(Sha256);
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer(Sha256::new());
    for event in session
        .events
        .iter()
        .filter(|event| event.line.is_some_and(|line| line < lines))
    {
        serde_json::to_writer(&mut writer, event).ok()?;
        writer.0.update(b"\n");
    }
    Some(hex::encode(writer.0.finalize()))
}

impl Prepared {
    pub(super) fn read(link: &Link, repo: &Repo, tip: Option<&str>) -> Option<Self> {
        if !super::settles_incrementally()
            || link.baseline_bytes.is_some()
            || !matches!(link.source.as_str(), "codex" | "claude-code")
        {
            return None;
        }
        let state = link.native_continuation.as_ref()?;
        let checkpoint = link.native_checkpoint.as_ref()?;
        if state.version != VERSION
            || Some(state.tip.as_str()) != tip
            || state.tip != checkpoint.tip
            || state.bytes != checkpoint.bytes
            || state.sha256 != checkpoint.sha256
            || state.source != link.source
            || state.session != link.session_id
            || Some(state.owner.as_str()) != link.owner.as_deref()
            || Some(state.repository.as_str()) != link.agent.as_deref()
            || Some(state.branch.as_str()) != link.branch.as_deref()
            || state.cwd != link.cwd
            || state.binding != link.native_binding
            || state.directory != directory(repo)?
            || state.origin >= state.bytes
            || state.titles.len() > 3
            || state.carriers.len() > 128
            || state.log != oid(repo, &state.tip, meta::LOG_FILE)?
            || state.view != oid(repo, &state.tip, meta::VIEW_FILE)?
        {
            return None;
        }
        let snapshot = meta::read_at_ref_result(repo, &state.tip).ok()??;
        if snapshot.layout != meta::LayoutVersion::V1
            || snapshot.session != state.claim
            || snapshot.is_file_line()
            || snapshot.turn.is_none()
            || snapshot.baseline_bytes != Some(state.bytes)
            || snapshot.runtime != link.source
        {
            return None;
        }
        let plan = carriers(link)?;
        if cut(&plan, state.bytes)? != state.carriers {
            return None;
        }
        let mut ranges = vec![Record {
            start: state.origin,
            end: state.bytes,
        }];
        ranges.extend(state.header.clone());
        ranges.extend(state.titles.iter().cloned());
        let mut facts = Facts::new(&link.source);
        let read = crate::adapter::native_snapshot::continuation::read(
            &plan,
            state.bytes,
            &state.sha256,
            &ranges,
            |bytes| facts.feed(bytes).is_some(),
        )
        .ok()??;
        if !facts.validates(state, snapshot.turn?) {
            return None;
        }
        let mut context = read.context.into_iter();
        let bytes = context.next()?;
        let context_len = usize::try_from(state.bytes.checked_sub(state.origin)?).ok()?;
        let header = if state.header.is_some() {
            Some(serde_json::from_slice(&context.next()?).ok()?)
        } else {
            None
        };
        let titles = context
            .map(String::from_utf8)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        let text = std::str::from_utf8(&bytes).ok()?;
        let context_text = std::str::from_utf8(&bytes[..context_len]).ok()?;
        if projection(
            &parse(
                &link.source,
                context_text,
                header.as_ref(),
                state.seen_message,
            )
            .ok()?,
            usize::MAX,
        )? != state.projection
        {
            return None;
        }
        for line in text.lines() {
            if serde_json::from_str::<serde_json::Value>(line)
                .is_ok_and(|value| unsupported(&value))
            {
                return None;
            }
        }
        let parsed = parse(&link.source, text, header.as_ref(), state.seen_message).ok()?;
        let lines = std::str::from_utf8(&bytes[..context_len])
            .ok()?
            .split_inclusive('\n')
            .count();
        if projection(&parsed, lines)? != state.projection {
            return None;
        }
        let settled = super::native::Settled::replay(
            repo,
            &state.tip,
            std::str::from_utf8(&bytes[..context_len]).ok()?,
            state.events_before,
            state.view_start,
        )
        .ok()?;
        Some(Self {
            bytes,
            state: state.clone(),
            context_len,
            header,
            titles,
            carriers: plan,
            settled,
            prefix: read.prefix,
            facts,
        })
    }

    pub(super) fn proves(&self, receipt: &super::StopReceipt) -> bool {
        receipt
            .bytes
            .checked_sub(self.state.origin as usize)
            .and_then(|end| self.hash(end))
            .is_some_and(|hash| hash == receipt.sha256)
    }

    pub(super) fn parse(&self, text: &str) -> crate::Result<Session> {
        parse(
            &self.state.source,
            text,
            self.header.as_ref(),
            self.state.seen_message,
        )
    }

    pub(super) fn hash(&self, end: usize) -> Option<String> {
        let suffix = self.bytes.get(self.context_len..end)?;
        let mut hash = self.prefix.clone();
        hash.update(suffix);
        Some(hex::encode(hash.finalize()))
    }
}

/// Seed acceleration only from a published native cut. The source ranges remain
/// selectors; the link contains neither prompt bodies nor parser event text.
pub(super) fn seed(
    link: &Link,
    repo: &Repo,
    bytes: &[u8],
    end: usize,
    prepared: Option<&Prepared>,
) -> Option<State> {
    let checkpoint = link.native_checkpoint.as_ref()?;
    if link.baseline_bytes.is_some() || !matches!(link.source.as_str(), "codex" | "claude-code") {
        return None;
    }
    let text = std::str::from_utf8(bytes.get(..end)?).ok()?;
    if !text.ends_with('\n') {
        return None;
    }
    let source_origin = prepared.map_or(0, |p| p.state.origin);
    let mut facts = prepared.map_or_else(|| Facts::new(&link.source), |p| p.facts.clone());
    facts.feed(bytes.get(prepared.map_or(0, |p| p.context_len)..end)?)?;
    if !facts.pending.is_empty() || !facts.modern || facts.position.bytes != checkpoint.bytes {
        return None;
    }
    let origin = facts.origin?;
    let local_origin = usize::try_from(origin.bytes.checked_sub(source_origin)?).ok()?;
    let header = facts.header_range();
    if link.source == "codex" && origin.bytes > 0 && header.is_none() {
        return None;
    }
    let titles = facts.title_ranges();
    let mut context_bytes = checkpoint.bytes.checked_sub(origin.bytes)?;
    for range in header.iter().chain(titles.iter()) {
        context_bytes = context_bytes.checked_add(range.end.checked_sub(range.start)?)?;
    }
    if context_bytes > CONTEXT_BYTES as u64 {
        return None;
    }
    let replay = parse(
        &link.source,
        text.get(local_origin..)?,
        header
            .as_ref()
            .and_then(|_| facts.header.as_ref().map(|(_, v)| v)),
        origin.seen,
    )
    .ok()?;
    let plan = prepared
        .map(|p| p.carriers.clone())
        .or_else(|| carriers(link))?;
    let snapshot = meta::read_at_ref_result(repo, &checkpoint.tip).ok()??;
    if snapshot.turn != Some(facts.position.prompts) {
        return None;
    }
    let state = State {
        version: VERSION,
        source: link.source.clone(),
        session: link.session_id.clone(),
        owner: link.owner.clone()?,
        repository: link.agent.clone()?,
        branch: link.branch.clone()?,
        directory: directory(repo)?,
        cwd: link.cwd.clone(),
        binding: link.native_binding.clone(),
        claim: snapshot.session,
        tip: checkpoint.tip.clone(),
        bytes: checkpoint.bytes,
        sha256: checkpoint.sha256.clone(),
        origin: origin.bytes,
        prompts_before: origin.prompts,
        events_before: origin.events,
        seen_message: origin.seen,
        max_timestamp: facts.max_timestamp,
        view_start: facts.view_start,
        log: oid(repo, &checkpoint.tip, meta::LOG_FILE)?,
        view: oid(repo, &checkpoint.tip, meta::VIEW_FILE)?,
        projection: projection(&replay, usize::MAX)?,
        header,
        titles,
        carriers: cut(&plan, checkpoint.bytes)?,
    };
    state.encode()?;
    Some(state)
}

fn title_index(value: &serde_json::Value) -> Option<usize> {
    match value["type"].as_str()? {
        "custom-title"
            if !value["customTitle"].as_str().is_some_and(|title| {
                crate::adapter::native_title::agit_placeholder(title.trim())
            }) =>
        {
            Some(0)
        }
        "ai-title" if value["aiTitle"].is_string() => Some(1),
        "summary" if value["summary"].is_string() => Some(2),
        _ => None,
    }
}
