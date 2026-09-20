//! Bounded, expiring snapshots keep a page chain independent of native writers.
use super::*;
use std::{
    collections::HashMap,
    io::Write,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SNAPSHOTS: usize = 8;
const LIFETIME: Duration = Duration::from_secs(600);

struct Snapshot {
    parts: Vec<Segment>,
    native_items: Option<Vec<Value>>,
    bytes: u64,
    _budget: OwnedSemaphorePermit,
}

struct CachedSnapshot {
    scope: String,
    snapshot: Arc<Mutex<Snapshot>>,
    touched: Instant,
}

struct CaptureState {
    finished: bool,
    result: Option<(String, Arc<Mutex<Snapshot>>)>,
}

struct InFlightCapture {
    state: Mutex<CaptureState>,
    ready: Condvar,
    waiters: AtomicUsize,
}

impl InFlightCapture {
    fn new() -> Self {
        Self {
            state: Mutex::new(CaptureState {
                finished: false,
                result: None,
            }),
            ready: Condvar::new(),
            waiters: AtomicUsize::new(0),
        }
    }

    fn wait(&self) -> crate::Result<Option<(String, Arc<Mutex<Snapshot>>)>> {
        self.waiters.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.lock().map_err(|_| Failure::Busy)?;
        while !state.finished {
            state = self.ready.wait(state).map_err(|_| Failure::Busy)?;
        }
        Ok(state.result.clone())
    }

    fn finish(&self, result: Option<(String, Arc<Mutex<Snapshot>>)>) -> crate::Result<()> {
        let mut state = self.state.lock().map_err(|_| Failure::Busy)?;
        state.finished = true;
        state.result = result;
        self.ready.notify_all();
        Ok(())
    }
}

struct Cache {
    entries: Mutex<HashMap<String, CachedSnapshot>>,
    inflight: Mutex<HashMap<String, Arc<InFlightCapture>>>,
    bytes: Arc<Semaphore>,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            bytes: Arc::new(Semaphore::new(MAX_TOTAL_BYTES as usize)),
        }
    }
}

impl Cache {
    fn snapshot(
        &self,
        scope: String,
        token: Option<&str>,
        capture: impl FnOnce() -> crate::Result<Snapshot>,
    ) -> crate::Result<(String, Arc<Mutex<Snapshot>>)> {
        if let Some(token) = token {
            let mut entries = self.entries.lock().map_err(|_| Failure::Busy)?;
            entries.retain(|_, entry| entry.touched.elapsed() < LIFETIME);
            let entry = entries.get_mut(token).context(Failure::Expired)?;
            ensure!(entry.scope == scope, Failure::Expired);
            entry.touched = Instant::now();
            return Ok((token.into(), entry.snapshot.clone()));
        }

        let mut capture = Some(capture);
        loop {
            let (flight, owner) = {
                let mut inflight = self.inflight.lock().map_err(|_| Failure::Busy)?;
                if let Some(flight) = inflight.get(&scope) {
                    (flight.clone(), false)
                } else {
                    let flight = Arc::new(InFlightCapture::new());
                    inflight.insert(scope.clone(), flight.clone());
                    (flight, true)
                }
            };

            if !owner {
                if let Some(result) = flight.wait()? {
                    return Ok(result);
                }
                // The original capture failed. Re-enter as the next owner so a
                // transient native writer race can be retried by a waiter.
                continue;
            }

            let result = (capture
                .take()
                .expect("a single-flight capture has one owner"))()
            .and_then(|snapshot| self.store(scope.clone(), snapshot));
            match result {
                Ok(result) => {
                    flight.finish(Some(result.clone()))?;
                    self.remove_inflight(&scope, &flight)?;
                    return Ok(result);
                }
                Err(error) => {
                    flight.finish(None)?;
                    self.remove_inflight(&scope, &flight)?;
                    return Err(error);
                }
            }
        }
    }

    fn store(
        &self,
        scope: String,
        snapshot: Snapshot,
    ) -> crate::Result<(String, Arc<Mutex<Snapshot>>)> {
        let snapshot = Arc::new(Mutex::new(snapshot));
        let token = uuid::Uuid::new_v4().to_string();
        let mut entries = self.entries.lock().map_err(|_| Failure::Busy)?;
        entries.retain(|_, entry| entry.touched.elapsed() < LIFETIME);
        while entries.len() >= MAX_SNAPSHOTS {
            let oldest = entries
                .iter()
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
                .context(Failure::Limit)?;
            entries.remove(&oldest);
        }
        entries.insert(
            token.clone(),
            CachedSnapshot {
                scope,
                snapshot: snapshot.clone(),
                touched: Instant::now(),
            },
        );
        Ok((token, snapshot))
    }

    fn remove_inflight(&self, scope: &str, flight: &Arc<InFlightCapture>) -> crate::Result<()> {
        let mut inflight = self.inflight.lock().map_err(|_| Failure::Busy)?;
        if inflight
            .get(scope)
            .is_some_and(|current| Arc::ptr_eq(current, flight))
        {
            inflight.remove(scope);
        }
        Ok(())
    }

    fn reserve(&self, bytes: u32) -> crate::Result<OwnedSemaphorePermit> {
        if let Ok(permit) = self.bytes.clone().try_acquire_many_owned(bytes) {
            return Ok(permit);
        }
        let mut entries = self.entries.lock().map_err(|_| Failure::Busy)?;
        // A live reader owns its byte reservation even after its cache entry is evicted.
        loop {
            if let Ok(permit) = self.bytes.clone().try_acquire_many_owned(bytes) {
                return Ok(permit);
            }
            let oldest = entries
                .iter()
                .filter(|(_, entry)| Arc::strong_count(&entry.snapshot) == 1)
                .min_by_key(|(_, entry)| entry.touched)
                .map(|(key, _)| key.clone())
                .context(Failure::Busy)?;
            entries.remove(&oldest);
        }
    }
}

static SNAPSHOTS: OnceLock<Cache> = OnceLock::new();

pub(super) fn read(
    runtime: &str,
    native: &str,
    cwd: &str,
    params: &Value,
    timings: &mut Timings,
) -> crate::Result<Value> {
    ensure!(
        params.get("before").is_none() || params.get("snapshot").is_some(),
        Failure::InvalidCursor
    );
    let token = params
        .get("snapshot")
        .map(|value| value.as_str().context(Failure::InvalidCursor))
        .transpose()?;
    let scope = json!([runtime, native, cwd]).to_string();
    let cache = SNAPSHOTS.get_or_init(Default::default);
    let (token, snapshot) = timings.measure("snapshot_ms", || {
        cache.snapshot(scope, token, || capture(runtime, native, cwd, cache))
    })?;
    // Only readers of the same immutable snapshot share file cursor positions.
    let mut entry = timings
        .measure("snapshot_lock_ms", || snapshot.lock())
        .map_err(|_| Failure::Busy)?;
    let before = params
        .get("before")
        .map(|value| value.as_u64().context(Failure::InvalidCursor))
        .transpose()?;
    let (items, next) = if let Some(items) = &entry.native_items {
        let end = before.unwrap_or(items.len() as u64);
        ensure!(end <= items.len() as u64, Failure::InvalidCursor);
        let start = end.saturating_sub(64);
        {
            let redactor = timings.measure("protection_context_ms", || {
                crate::rc::protection::for_native(runtime, native, std::path::Path::new(cwd))
            })?;
            let page: Vec<Value> = timings.measure("projection_ms", || {
                items[start as usize..end as usize]
                    .iter()
                    .map(|item| -> crate::Result<Value> {
                        let mut item = item.clone();
                        let scrubbed = redactor.try_scrub_json(&item["raw"])?;
                        item["raw"] = scrubbed.value;
                        item["event"] = redactor.try_scrub_json(&item["event"])?.value;
                        if scrubbed.secrets > 0 {
                            item["object_hash"] =
                                crate::domain::transcript::object_hash(&item["raw"]).into();
                        }
                        Ok(item)
                    })
                    .collect::<crate::Result<_>>()
            })?;
            (page, start)
        }
    } else {
        let (lines, next, mode, context) = timings.measure("page_ms", || -> crate::Result<_> {
            let (mut lines, next, mode) = page_segments(&mut entry.parts, before)?;
            validate_records(runtime, &lines)?;
            let context = select_view(&mut lines, runtime, params);
            Ok((lines, next, mode, context))
        })?;
        let redactor = timings.measure("protection_context_ms", || {
            crate::rc::protection::for_native(runtime, native, std::path::Path::new(cwd))
        })?;
        let items: Vec<Value> = timings.measure("projection_ms", || {
            let (items, _) = super::super::supervisor::items_from_lines_with_mode(
                runtime, &redactor, &lines, mode,
            );
            items
                .into_iter()
                .map(|mut item| {
                    if runtime == "codex" && params["view"] == "conversation" {
                        project_context(&mut item, &context);
                    }
                    item.event.line = None;
                    json!({
                        "item_id": format!("history:{}", item.item_id),
                        "source_id": item.source_id,
                        "event": item.event,
                        "raw": item.raw,
                    })
                })
                .collect()
        });
        (items, next)
    };
    let result =
        json!({"items":items,"before":next,"has_more":next>0,"snapshot":token,"status":"complete"});
    ensure!(
        timings
            .measure("size_check_ms", || serde_json::to_vec(&result))?
            .len()
            < super::super::local::MAX_FRAME - 1024,
        Failure::Limit
    );
    Ok(result)
}

fn capture(runtime: &str, native: &str, cwd: &str, cache: &Cache) -> crate::Result<Snapshot> {
    let mut snapshot = Snapshot {
        _budget: cache.reserve(0)?,
        parts: vec![],
        native_items: None,
        bytes: 0,
    };
    if runtime == "opencode" {
        snapshot._budget.merge(cache.reserve(MAX_BYTES as u32)?);
        use crate::adapter::{Adapter, native_snapshot::Limits, opencode::OpenCode};
        let source = OpenCode.lookup_native_readonly(native, Limits::default())?;
        let bytes = super::super::supervisor::native_records::read_watch_snapshot_blocking(
            &source,
            std::path::Path::new(cwd),
        )?;
        snapshot.bytes = bytes.len() as u64;
        let redactor =
            crate::rc::protection::for_native(runtime, native, std::path::Path::new(cwd))?;
        let (items, _) = super::super::supervisor::native_records::NativeRecords::default()
            .project(&bytes, false, &redactor)?;
        let items = items
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()?;
        snapshot.bytes += serde_json::to_vec(&items)?.len() as u64;
        ensure!(snapshot.bytes <= MAX_BYTES, Failure::Limit);
        drop(
            snapshot
                ._budget
                .split((MAX_BYTES - snapshot.bytes) as usize),
        );
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
            snapshot._budget.merge(cache.reserve(part.end as u32)?);
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
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::mpsc,
        thread,
        time::{Duration, Instant},
    };

    #[test]
    fn captures_and_readers_do_not_lock_unrelated_history_or_release_live_bytes() {
        let cache = Cache::default();
        let make = || {
            Ok(Snapshot {
                parts: vec![],
                native_items: None,
                bytes: 1,
                _budget: cache.reserve(1)?,
            })
        };
        let (token, first) = cache
            .snapshot("first".into(), None, || {
                let (_, other) = cache.snapshot("other".into(), None, make)?;
                assert_eq!(other.lock().unwrap().bytes, 1);
                make()
            })
            .unwrap();
        let reading = first.lock().unwrap();
        let (_, second) = cache.snapshot("second".into(), None, make).unwrap();
        assert_eq!(second.lock().unwrap().bytes, 1);
        assert!(
            cache
                .snapshot("another scope".into(), Some(&token), make)
                .is_err()
        );
        assert!(Arc::ptr_eq(
            &first,
            &cache
                .snapshot("first".into(), Some(&token), make)
                .unwrap()
                .1
        ));
        cache.entries.lock().unwrap().clear();
        assert_eq!(
            cache.bytes.available_permits(),
            MAX_TOTAL_BYTES as usize - 2
        );
        drop(reading);
        drop(first);
        drop(second);
        assert_eq!(cache.bytes.available_permits(), MAX_TOTAL_BYTES as usize);
        assert!(cache.snapshot("first".into(), Some(&token), make).is_err());
        let full = cache.reserve(MAX_TOTAL_BYTES as u32).unwrap();
        assert!(cache.reserve(1).is_err());
        drop(full);
        assert!(cache.reserve(1).is_ok());
    }

    #[test]
    fn concurrent_requests_for_one_scope_share_capture() {
        let cache = Arc::new(Cache::default());
        let captures = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let leader_cache = cache.clone();
        let leader_captures = captures.clone();
        let leader = thread::spawn(move || {
            leader_cache.snapshot("scope".into(), None, || {
                leader_captures.fetch_add(1, Ordering::Relaxed);
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(Snapshot {
                    parts: vec![],
                    native_items: None,
                    bytes: 1,
                    _budget: leader_cache.reserve(1)?,
                })
            })
        });
        started_rx.recv().unwrap();

        let (follower_entered_tx, follower_entered_rx) = mpsc::channel();
        let follower_cache = cache.clone();
        let follower_captures = captures.clone();
        let follower = thread::spawn(move || {
            follower_entered_tx.send(()).unwrap();
            follower_cache.snapshot("scope".into(), None, || {
                follower_captures.fetch_add(1, Ordering::Relaxed);
                Ok(Snapshot {
                    parts: vec![],
                    native_items: None,
                    bytes: 1,
                    _budget: follower_cache.reserve(1)?,
                })
            })
        });
        follower_entered_rx.recv().unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let flight = cache
                .inflight
                .lock()
                .unwrap()
                .get("scope")
                .cloned()
                .unwrap();
            if flight.waiters.load(Ordering::Relaxed) > 0 {
                break;
            }
            assert!(Instant::now() < deadline, "follower did not join capture");
            thread::yield_now();
        }

        release_tx.send(()).unwrap();
        let (leader_token, leader_snapshot) = leader.join().unwrap().unwrap();
        let (follower_token, follower_snapshot) = follower.join().unwrap().unwrap();
        assert_eq!(captures.load(Ordering::Relaxed), 1);
        assert_eq!(leader_token, follower_token);
        assert!(Arc::ptr_eq(&leader_snapshot, &follower_snapshot));
    }
}
