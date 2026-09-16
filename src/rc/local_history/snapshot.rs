//! Bounded, expiring snapshots keep a page chain independent of native writers.
use super::*;
use std::{
    collections::HashMap,
    io::Write,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SNAPSHOTS: usize = 8;
const LIFETIME: Duration = Duration::from_secs(600);

struct Snapshot {
    scope: String,
    parts: Vec<Segment>,
    native_items: Option<Vec<Value>>,
    bytes: u64,
    touched: Instant,
}

static SNAPSHOTS: OnceLock<Mutex<HashMap<String, Snapshot>>> = OnceLock::new();

pub(super) fn read(runtime: &str, native: &str, cwd: &str, params: &Value) -> crate::Result<Value> {
    let mut cache = SNAPSHOTS
        .get_or_init(Default::default)
        .try_lock()
        .map_err(|_| Failure::Busy)?;
    cache.retain(|_, entry| entry.touched.elapsed() < LIFETIME);
    let scope = json!([runtime, native, cwd]).to_string();
    ensure!(
        params.get("before").is_none() || params.get("snapshot").is_some(),
        Failure::InvalidCursor
    );
    let token = if let Some(token) = params.get("snapshot") {
        let token = token.as_str().context(Failure::InvalidCursor)?;
        ensure!(
            cache.get(token).is_some_and(|entry| entry.scope == scope),
            Failure::Expired
        );
        token.to_string()
    } else {
        let (token, snapshot) = capture(runtime, native, cwd, scope)?;
        while !cache.contains_key(&token)
            && (cache.len() >= MAX_SNAPSHOTS
                || cache.values().map(|entry| entry.bytes).sum::<u64>() + snapshot.bytes
                    > MAX_TOTAL_BYTES)
        {
            let oldest = cache
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
                .context(Failure::Limit)?;
            cache.remove(&oldest);
        }
        cache.insert(token.clone(), snapshot);
        token
    };
    let entry = cache.get_mut(&token).context(Failure::Expired)?;
    entry.touched = Instant::now();
    let before = params
        .get("before")
        .map(|value| value.as_u64().context(Failure::InvalidCursor))
        .transpose()?;
    let (items, next) =
        if let Some(items) = &entry.native_items {
            let end = before.unwrap_or(items.len() as u64);
            ensure!(end <= items.len() as u64, Failure::InvalidCursor);
            let start = end.saturating_sub(64);
            {
                let redactor = crate::domain::redact::Redactor::try_this_machine()?;
                let page: Vec<Value> = items[start as usize..end as usize]
                    .iter()
                    .map(|item| {
                        let scrubbed = redactor.scrub_json(item);
                        let mut item = scrubbed.value;
                        if !scrubbed.registered_ids.is_empty() {
                            item["object_hash"] =
                                crate::domain::transcript::object_hash(&item["raw"]).into();
                        }
                        item
                    })
                    .collect();
                (page, start)
            }
        } else {
            let (lines, next, mode) = page_segments(&mut entry.parts, before)?;
            validate_records(runtime, &lines)?;
            let redactor = crate::domain::redact::Redactor::try_this_machine()?;
            let (items, _) = super::super::supervisor::items_from_lines_with_mode(
                runtime, &redactor, &lines, mode,
            );
            (items.into_iter().map(|mut item| {
            item.event.line = None;
            json!({"item_id":format!("history:{}",item.item_id),"source_id":item.source_id,
                "event":item.event,"raw":item.raw})
        }).collect(), next)
        };
    let result =
        json!({"items":items,"before":next,"has_more":next>0,"snapshot":token,"status":"complete"});
    ensure!(
        serde_json::to_vec(&result)?.len() < super::super::local::MAX_FRAME - 1024,
        Failure::Limit
    );
    Ok(result)
}

fn capture(
    runtime: &str,
    native: &str,
    cwd: &str,
    scope: String,
) -> crate::Result<(String, Snapshot)> {
    let mut snapshot = Snapshot {
        scope,
        parts: vec![],
        native_items: None,
        bytes: 0,
        touched: Instant::now(),
    };
    if runtime == "opencode" {
        use crate::adapter::{Adapter, native_snapshot::Limits, opencode::OpenCode};
        let source = OpenCode.lookup_native_readonly(native, Limits::default())?;
        let bytes = super::super::supervisor::native_records::read_watch_snapshot_blocking(
            &source,
            std::path::Path::new(cwd),
        )?;
        snapshot.bytes = bytes.len() as u64;
        let redactor = crate::domain::redact::Redactor::try_this_machine()?;
        let (items, _) = super::super::supervisor::native_records::NativeRecords::default()
            .project(&bytes, false, &redactor)?;
        let items = items
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?;
        snapshot.bytes += serde_json::to_vec(&items)?.len() as u64;
        ensure!(snapshot.bytes <= MAX_BYTES, Failure::Limit);
        snapshot.native_items = Some(items);
    } else {
        let path = crate::adapter::get(runtime)?
            .resolve(native, Some(std::path::Path::new(cwd)))
            .ok_or(Failure::Missing)?;
        let parts = if runtime == "codex" {
            lineage(&path)?
        } else {
            let file = std::fs::File::open(&path)?;
            vec![Segment {
                end: file.metadata()?.len(),
                version: file.metadata()?,
                file,
                source: path,
            }]
        };
        for mut part in parts {
            snapshot.bytes = snapshot
                .bytes
                .checked_add(part.end)
                .context(Failure::Limit)?;
            ensure!(snapshot.bytes <= MAX_BYTES, Failure::Limit);
            if part.end > 0 {
                part.file.seek(SeekFrom::Start(part.end - 1))?;
                let mut delimiter = [0];
                part.file.read_exact(&mut delimiter)?;
                ensure!(delimiter == *b"\n", Failure::Incomplete);
            }
            let before = part.version;
            let mut copy = tempfile::tempfile()?;
            part.file.seek(SeekFrom::Start(0))?;
            let mut remaining = part.end;
            let mut buffer = [0; 64 * 1024];
            while remaining > 0 {
                let count = remaining.min(buffer.len() as u64) as usize;
                part.file.read_exact(&mut buffer[..count])?;
                copy.write_all(&buffer[..count])?;
                remaining -= count as u64;
            }
            let after = part.file.metadata()?;
            ensure!(
                before.len() == after.len() && before.modified()? == after.modified()?,
                Failure::Changed
            );
            snapshot.parts.push(Segment {
                version: copy.metadata()?,
                file: copy,
                end: part.end,
                source: part.source,
            });
        }
    }
    Ok((uuid::Uuid::new_v4().to_string(), snapshot))
}
