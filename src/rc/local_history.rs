//! Read-only pages addressed by native transcript byte boundaries.
mod snapshot;

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};

#[derive(Default, serde::Serialize)]
pub(crate) struct Timings(std::collections::BTreeMap<&'static str, f64>);

impl Timings {
    fn measure<T>(&mut self, phase: &'static str, operation: impl FnOnce() -> T) -> T {
        let started = std::time::Instant::now();
        let result = operation();
        self.0
            .insert(phase, started.elapsed().as_secs_f64() * 1000.0);
        result
    }
}

pub fn read(params: Value) -> crate::Result<Value> {
    read_timed(params, &mut Timings::default())
}

pub(crate) fn read_timed(params: Value, timings: &mut Timings) -> crate::Result<Value> {
    let roster = timings.measure("roster_ms", super::roster::Roster::try_load)?;
    read_with_roster(params, &roster, timings)
}

fn read_with_roster(
    params: Value,
    roster: &super::roster::Roster,
    timings: &mut Timings,
) -> crate::Result<Value> {
    let session = params["session_id"]
        .as_str()
        .context("Session id is required")?;
    let entry = roster.get(session);
    let runtime = entry
        .map(|e| e.runtime.as_str())
        .or_else(|| params["runtime"].as_str())
        .context("Runtime is required")?;
    let native = entry.map(|e| e.thread_id.as_str()).unwrap_or(session);
    ensure!(
        matches!(runtime, "codex" | "claude-code" | "opencode"),
        Failure::Unsupported
    );
    ensure!(
        !native.is_empty()
            && native
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c)),
        "Invalid native session id"
    );
    let cwd = entry
        .map(|e| e.cwd.as_str())
        .or_else(|| params["cwd"].as_str())
        .context("Working directory is required")?;
    snapshot::read(runtime, native, cwd, &params, timings)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Failure {
    #[error("Native transcript is unavailable")]
    Missing,
    #[error("Native history ends in an incomplete record; retry after the writer finishes")]
    Incomplete,
    #[error("This runtime does not support history paging")]
    Unsupported,
    #[error("History reader is busy; retry shortly")]
    Busy,
    #[error("History snapshot expired; reload history")]
    Expired,
    #[error("Invalid history cursor; reload history")]
    InvalidCursor,
    #[error("History exceeds the bounded read budget")]
    Limit,
    #[error("Native history changed during capture; retry")]
    Changed,
    #[error("Native history contains an invalid record")]
    Corrupt,
}

pub(crate) fn rpc_error(error: anyhow::Error) -> crate::protocol::RpcError {
    use crate::protocol::{ErrorCode, RpcError};
    let (kind, retryable, restart) = match error.downcast_ref::<Failure>() {
        Some(Failure::Missing) => ("source_missing", false, true),
        Some(Failure::Incomplete) => ("incomplete_record", true, false),
        Some(Failure::Unsupported) => ("unsupported", false, false),
        Some(Failure::Busy) => ("busy", true, false),
        Some(Failure::Expired | Failure::InvalidCursor) => ("cursor_expired", false, true),
        Some(Failure::Limit) => ("resource_limit", false, false),
        Some(Failure::Changed) => ("source_changed", true, true),
        Some(Failure::Corrupt) => ("invalid_record", false, false),
        None if error
            .downcast_ref::<crate::adapter::native_snapshot::Unavailable>()
            .is_some() =>
        {
            use crate::adapter::native_snapshot::Unavailable;
            match error.downcast_ref::<Unavailable>().unwrap() {
                Unavailable::NotFound => ("source_missing", false, true),
                Unavailable::Unsupported => ("unsupported", false, false),
                Unavailable::BudgetExceeded => ("resource_limit", false, false),
                Unavailable::Incomplete => ("incomplete_record", true, false),
                Unavailable::Changed => ("source_changed", true, true),
                _ => ("read_failed", true, false),
            }
        }
        None => match error
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind)
        {
            Some(std::io::ErrorKind::NotFound) => ("source_missing", false, true),
            Some(std::io::ErrorKind::PermissionDenied) => ("source_unreadable", false, false),
            _ => ("read_failed", true, false),
        },
    };
    // Native paths and parser excerpts are not part of the remote error contract.
    let mut result = RpcError::new(
        ErrorCode::RuntimeUnavailable,
        format!("History read failed ({kind})"),
    );
    result.data = Some(json!({"kind":kind,"retryable":retryable,"restart":restart}));
    result
}

pub(crate) fn validate_records(
    runtime: &str,
    lines: &[super::tail::TailedLine],
) -> crate::Result<()> {
    let adapter = crate::adapter::get(runtime)?;
    for line in lines.iter().filter(|line| !line.text.trim().is_empty()) {
        serde_json::from_str::<Value>(&line.text).map_err(|_| Failure::Corrupt)?;
        if runtime != "codex" {
            adapter.parse(&line.text).map_err(|_| Failure::Corrupt)?;
        }
    }
    Ok(())
}

fn select_view(
    lines: &mut Vec<super::tail::TailedLine>,
    runtime: &str,
    params: &Value,
) -> HashSet<u64> {
    let mut context = HashSet::new();
    if runtime != "codex" || params["view"] != "conversation" {
        return context;
    }
    // Filter after pagination so native byte cursors and retained item identities stay stable.
    lines.retain(|line| {
        let Ok(raw) = serde_json::from_str::<Value>(&line.text) else {
            return true;
        };
        // Classify source records before raw payload caps remove their message roles.
        if raw["type"] == "response_item"
            && raw["payload"]["type"] == "message"
            && matches!(
                raw["payload"]["role"].as_str(),
                Some("system" | "developer")
            )
        {
            context.insert(line.lineno);
        }
        match raw["type"].as_str() {
            Some("session_meta" | "turn_context" | "world_state" | "token_usage_record") => false,
            Some("event_msg") => raw["payload"]["type"] != "token_count",
            _ => true,
        }
    });
    context
}

fn project_context(item: &mut crate::protocol::ItemCompleted, context: &HashSet<u64>) {
    if item.event.kind == crate::adapter::EventKind::Other && context.contains(&item.line) {
        // Keep message boundaries and native IDs for compaction and live reconciliation.
        item.event.text = None;
        if let Some(content) = item.raw.pointer_mut("/payload/content") {
            *content = json!([]);
        }
    }
}

struct Segment {
    source: std::path::PathBuf,
    version: std::fs::Metadata,
    file: std::fs::File,
    end: u64,
}

// Virtual byte coordinates join immutable parent prefixes to the growing leaf.
fn lineage(path: &std::path::Path) -> crate::Result<Vec<Segment>> {
    lineage_from(path, |id| {
        Ok(crate::adapter::native_snapshot::lookup_codex_rollout(
            id,
            crate::adapter::native_snapshot::Limits::default(),
        )?
        .path)
    })
}

fn lineage_from(
    path: &std::path::Path,
    lookup: impl Fn(&str) -> crate::Result<std::path::PathBuf>,
) -> crate::Result<Vec<Segment>> {
    use std::io::BufRead;
    let mut segments = Vec::new();
    let mut current = path.to_path_buf();
    let mut boundary = None;
    let mut seen = std::collections::HashSet::new();
    loop {
        ensure!(
            segments.len() < 64 && seen.insert(current.clone()),
            "Native history lineage is cyclic or too deep"
        );
        let mut file = std::fs::File::open(&current)?;
        let version = file.metadata()?;
        let length = version.len();
        let end = boundary.map(|(bytes, _)| bytes).unwrap_or(length);
        ensure!(end <= length, "Native parent history is incomplete");
        let mut header = String::new();
        std::io::BufReader::new((&mut file).take(1024 * 1024)).read_line(&mut header)?;
        let value = if header.ends_with('\n') {
            serde_json::from_str::<Value>(&header).ok()
        } else {
            None
        };
        if let Some((_, ordinal)) = boundary {
            ensure!(
                end >= header.len() as u64 && end > 0,
                "Native parent boundary excludes its header"
            );
            file.seek(SeekFrom::Start(end - 1))?;
            let mut delimiter = [0];
            file.read_exact(&mut delimiter)?;
            ensure!(
                delimiter == *b"\n",
                "Native parent boundary splits a record"
            );
            let (records, _, _) = page_file(&mut file, Some(end))?;
            let last: Value = serde_json::from_str(
                &records
                    .last()
                    .context("Native parent boundary is empty")?
                    .text,
            )?;
            ensure!(
                last["ordinal"].as_u64().and_then(|v| v.checked_add(1)) == Some(ordinal),
                "Native parent history boundary changed"
            );
        }
        segments.push(Segment {
            version,
            file,
            end,
            source: current.clone(),
        });
        let Some(value) = value else {
            ensure!(boundary.is_none(), "Native parent header is invalid");
            break;
        };
        if value["type"] != "session_meta" {
            break;
        }
        let base = &value["payload"]["history_base"];
        if value["payload"]["history_mode"] != "paginated" || base.is_null() {
            break;
        }
        let id = base["thread_id"]
            .as_str()
            .context("Native parent identity is missing")?;
        boundary = Some((
            base["end_byte_offset"]
                .as_u64()
                .context("Native parent byte boundary is missing")?,
            base["end_ordinal_exclusive"]
                .as_u64()
                .context("Native parent ordinal boundary is missing")?,
        ));
        current = lookup(id)?;
    }
    segments.reverse();
    Ok(segments)
}

pub(crate) fn watch_cursor(path: &std::path::Path, offset: u64) -> crate::Result<u64> {
    let segments = lineage(path)?;
    segments
        .iter()
        .take(segments.len().saturating_sub(1))
        .try_fold(offset, |total, part| {
            total
                .checked_add(part.end)
                .context("Native history is too large")
        })
}

fn page_segments(
    segments: &mut [Segment],
    before: Option<u64>,
) -> crate::Result<(
    Vec<super::tail::TailedLine>,
    u64,
    super::codex_history::HistoryMode,
)> {
    let total = segments.iter().try_fold(0u64, |n, part| {
        n.checked_add(part.end)
            .context("Native history is too large")
    })?;
    let before = before.unwrap_or(total);
    ensure!(before <= total, Failure::InvalidCursor);
    let mut base = total;
    for part in segments.iter_mut().rev() {
        base -= part.end;
        if before <= base {
            continue;
        }
        let (mut lines, next, mode) = page_file(&mut part.file, Some(before - base))?;
        for line in &mut lines {
            line.source = Some(super::tail::record_source(&part.source, line.lineno));
            line.lineno += base;
        }
        return Ok((lines, base + next, mode));
    }
    Ok((vec![], 0, super::codex_history::HistoryMode::Model))
}

#[cfg(test)]
fn page(
    path: &std::path::Path,
    before: Option<u64>,
) -> crate::Result<(
    Vec<super::tail::TailedLine>,
    u64,
    super::codex_history::HistoryMode,
)> {
    let mut file = std::fs::File::open(path)?;
    page_file(&mut file, before)
}

fn page_file(
    file: &mut std::fs::File,
    before: Option<u64>,
) -> crate::Result<(
    Vec<super::tail::TailedLine>,
    u64,
    super::codex_history::HistoryMode,
)> {
    // Header and page must share an open file so replacement cannot mix history modes.
    let mode = super::codex_history::read_header(file).mode();
    let length = file.metadata()?.len();
    let end = before.unwrap_or(length);
    ensure!(
        end <= length,
        "Transcript changed; reopen the conversation to reload history"
    );
    if before.is_some() && end > 0 {
        file.seek(SeekFrom::Start(end - 1))?;
        let mut delimiter = [0];
        file.read_exact(&mut delimiter)?;
        ensure!(delimiter == *b"\n", Failure::Changed);
    }
    if end == 0 {
        return Ok((vec![], 0, mode));
    }
    let mut window = 512 * 1024;
    let (start, bytes, boundaries) = loop {
        let start = end.saturating_sub(window);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = vec![0; (end - start) as usize];
        file.read_exact(&mut bytes)?;
        let mut boundaries = vec![];
        if start == 0 {
            boundaries.push(0);
        }
        for (index, byte) in bytes.iter().enumerate() {
            if *byte == b'\n' {
                boundaries.push(index + 1);
            }
        }
        if boundaries.len() >= 2 {
            break (start, bytes, boundaries);
        }
        ensure!(
            start > 0 && window < 128 * 1024 * 1024,
            "A native transcript record exceeds the history page limit"
        );
        window *= 2;
    };
    let last = *boundaries.last().unwrap();
    let mut first = boundaries.len() - 2;
    while first > 0 && boundaries.len() - first <= 65 && last - boundaries[first - 1] <= 512 * 1024
    {
        first -= 1;
    }
    let mut lines = vec![];
    for range in boundaries[first..].windows(2) {
        lines.push(super::tail::TailedLine {
            source: None,
            lineno: start + range[0] as u64,
            text: std::str::from_utf8(&bytes[range[0]..range[1]])?
                .trim_end_matches('\n')
                .to_string(),
        });
    }
    Ok((lines, start + boundaries[first] as u64, mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn conversation_view_preserves_native_pages_and_presentable_records() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.jsonl");
        let mut records = vec![
            json!({"type":"session_meta","payload":{"id":"fixture","history_mode":"model"}}),
            json!({"type":"event_msg","payload":{"type":"token_count"}}),
            json!({"type":"world_state","payload":{"context":"Context"}}),
            json!({"type":"token_usage_record","payload":{"tokens":1}}),
        ];
        let visible = vec![
            json!({"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"Context"}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"system","content":[{"type":"input_text","text":"X".repeat(300 * 1024)}]}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Question"}]}}),
            json!({"type":"response_item","payload":{"type":"reasoning","summary":[{"text":"Thinking"}]}}),
            json!({"type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"call","arguments":"{}"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call","output":"Done"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Answer"}]}}),
            json!({"type":"compacted","payload":{"message":"Summary"}}),
            json!({"type":"future_record","payload":{"text":"Preserved"}}),
        ];
        records.extend(visible.clone());
        records.extend((0..100).map(|_| json!({"type":"turn_context","payload":{}})));
        std::fs::write(
            &path,
            records
                .iter()
                .map(|row| format!("{row}\n"))
                .collect::<String>(),
        )
        .unwrap();
        let (mut tail, before, _) = page(&path, None).unwrap();
        select_view(&mut tail, "codex", &json!({"view":"conversation"}));
        assert!(tail.is_empty());
        assert!(before > 0);
        let (mut earlier, before, mode) = page(&path, Some(before)).unwrap();
        assert_eq!(before, 0);
        let original: Vec<_> = earlier
            .iter()
            .map(|line| (line.lineno, line.text.clone()))
            .collect();
        select_view(&mut earlier, "codex", &json!({}));
        assert_eq!(earlier.len(), original.len());
        select_view(&mut earlier, "claude-code", &json!({"view":"conversation"}));
        assert_eq!(earlier.len(), original.len());
        let redactor =
            crate::domain::redact::Redactor::new(crate::domain::redact::Persona::default());
        let (full, _) = super::super::supervisor::items_from_lines_with_mode(
            "codex", &redactor, &earlier, mode,
        );
        let context = select_view(&mut earlier, "codex", &json!({"view":"conversation"}));
        assert_eq!(
            earlier
                .iter()
                .map(|line| serde_json::from_str::<Value>(&line.text).unwrap())
                .collect::<Vec<_>>(),
            visible
        );
        let (projected, _) = super::super::supervisor::items_from_lines_with_mode(
            "codex", &redactor, &earlier, mode,
        );
        assert!(!projected.is_empty());
        for mut item in projected {
            let expected = full
                .iter()
                .find(|original| original.item_id == item.item_id)
                .unwrap();
            assert_eq!(
                serde_json::to_value(&item).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
            project_context(&mut item, &context);
            if context.contains(&expected.line) {
                assert!(item.event.text.is_none());
                let mut raw = expected.raw.clone();
                if let Some(content) = raw.pointer_mut("/payload/content") {
                    *content = json!([]);
                } else {
                    assert!(expected.raw_truncated);
                    assert_eq!(expected.event.text.as_ref().unwrap().len(), 300 * 1024);
                }
                assert_eq!(item.raw, raw);
                assert_eq!(item.item_id, expected.item_id);
                assert_eq!(item.object_hash, expected.object_hash);
                assert_eq!(item.source_id, expected.source_id);
                assert_eq!(item.raw_truncated, expected.raw_truncated);
            } else {
                assert_eq!(
                    serde_json::to_value(item).unwrap(),
                    serde_json::to_value(expected).unwrap()
                );
            }
        }
    }

    #[test]
    fn launch_receipts_do_not_prove_that_missing_history_is_pending() {
        let session = json!({"session_id":"logical","workspace_id":"local-owner","runtime":"claude-code","status":"idle","last_seq":0,"created_at":"now","updated_at":"now"});
        let spec = json!({"workspace_id":"local-owner","project_id":"project","runtime":"claude-code","cwd":"/fixture","permission_mode":"default"});
        for state in [
            json!({"state":"pending","session":session}),
            json!({"state":"completed","result":{"session":session}}),
        ] {
            let mut roster: super::super::roster::Roster =
                serde_json::from_value(json!({"starts":{"launch":{"spec":spec,"state":state}}}))
                    .unwrap();
            assert!(
                read_with_roster(
                    json!({"session_id":"logical","runtime":"claude-code","cwd":"/fixture"}),
                    &roster,
                    &mut Timings::default(),
                )
                .is_err()
            );
            for native in [String::new(), uuid::Uuid::new_v4().to_string()] {
                roster.sessions.insert("logical".into(), serde_json::from_value(json!({"runtime":"claude-code","thread_id":native,"cwd":"/fixture","workspace_id":"local-owner"})).unwrap());
                for before in [Value::Null, json!(100)] {
                    assert!(
                        read_with_roster(
                            json!({"session_id":"logical","before":before}),
                            &roster,
                            &mut Timings::default(),
                        )
                        .is_err()
                    );
                }
            }
        }
    }

    #[test]
    fn completed_launch_reports_history_removed_after_restart() {
        const CHILD: &str = "AGIT_TEST_REMOVED_NATIVE_HISTORY";
        let Some(root) = std::env::var_os(CHILD).map(std::path::PathBuf::from) else {
            let directory = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "rc::local_history::tests::completed_launch_reports_history_removed_after_restart", "--nocapture"])
                .env(CHILD, directory.path())
                .env("AGIT_HOME", directory.path().join("agit"))
                .env("CLAUDE_CONFIG_DIR", directory.path().join("claude"))
                .output().unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(directory.path().join("completed").exists());
            return;
        };
        let native = uuid::Uuid::new_v4().to_string();
        let project = crate::adapter::claude_code::projects_dir()
            .unwrap()
            .join(crate::adapter::claude_code::slug_for(&root));
        std::fs::create_dir_all(&project).unwrap();
        let path = project.join(format!("{native}.jsonl"));
        let credential = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        std::fs::write(
            &path,
            format!(
                "{}\n",
                json!({"type":"user","message":{"role":"user","content":format!("Existing native history {credential}")}})
            ),
        )
        .unwrap();
        let session = json!({"session_id":"logical","workspace_id":"local-owner","runtime":"claude-code","status":"idle","last_seq":0,"created_at":"now","updated_at":"now"});
        let roster: super::super::roster::Roster = serde_json::from_value(json!({
            "sessions":{"logical":{"runtime":"claude-code","thread_id":native,"cwd":root,"workspace_id":"local-owner"}},
            "starts":{"launch":{"spec":{"workspace_id":"local-owner","project_id":"project","runtime":"claude-code","cwd":root,"permission_mode":"default"},"state":{"state":"completed","result":{"session":session}}}}
        })).unwrap();
        let params = json!({"session_id":"logical"});
        let history = read_with_roster(params.clone(), &roster, &mut Timings::default()).unwrap();
        assert!(history["items"].as_array().unwrap().iter().any(|item| {
            item["event"]["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("Existing native history "))
        }));
        assert!(!history.to_string().contains(credential));
        assert!(!history.to_string().contains("protection_error"));
        roster.save().unwrap();
        drop(roster);
        let restarted = super::super::roster::Roster::try_load().unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(
            read_with_roster(params, &restarted, &mut Timings::default())
                .unwrap_err()
                .to_string(),
            "Native transcript is unavailable"
        );
        assert!(!restarted.starts.is_empty());
        std::fs::write(root.join("completed"), []).unwrap();
    }

    #[test]
    fn lineage_keeps_open_sources_when_a_path_is_replaced() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("history.jsonl");
        let replacement = root.path().join("replacement.jsonl");
        std::fs::write(&path, "{\"type\":\"session_meta\",\"payload\":{\"id\":\"test\",\"history_mode\":\"legacy\"}}\noriginal\n").unwrap();
        let mut segments = lineage_from(&path, |_| anyhow::bail!("No parent expected")).unwrap();
        std::fs::write(&replacement, "replacement\n").unwrap();
        std::fs::rename(replacement, &path).unwrap();
        let (rows, before, mode) = page_segments(&mut segments, None).unwrap();
        assert_eq!(before, 0);
        assert_eq!(mode, super::super::codex_history::HistoryMode::Legacy);
        assert_eq!(rows.last().unwrap().text, "original");
    }

    #[test]
    fn cyclic_and_partial_parent_boundaries_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("history.jsonl");
        let header = |bytes| {
            format!(
                "{}\n",
                json!({"type":"session_meta","ordinal":0,"payload":{"id":"test","history_mode":"paginated","history_base":{"thread_id":"test","end_byte_offset":bytes,"end_ordinal_exclusive":1}}})
            )
        };
        std::fs::write(&path, header(1)).unwrap();
        let error = lineage_from(&path, |_| Ok(path.clone())).err().unwrap();
        assert!(error.to_string().contains("cyclic"));
        let child = root.path().join("child.jsonl");
        std::fs::write(&path, "{\"type\":\"session_meta\",\"ordinal\":0,\"payload\":{\"id\":\"test\"}}\n{\"ordinal\":1}\n").unwrap();
        std::fs::write(&child, header(std::fs::metadata(&path).unwrap().len() - 1)).unwrap();
        let error = lineage_from(&child, |_| Ok(path.clone())).err().unwrap();
        assert!(error.to_string().contains("splits a record"));
    }

    #[test]
    fn inherited_pages_preserve_source_order_and_stop_at_the_captured_parent_boundary() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent.jsonl");
        let child = root.path().join("child.jsonl");
        let row = |ordinal, text: &str| {
            format!(
                "{}\n",
                json!({"ordinal":ordinal,"type":"event_msg","payload":{"type":"user_message","message":text}})
            )
        };
        let header = format!(
            "{}\n",
            json!({"ordinal":0,"type":"session_meta","payload":{"id":"parent","history_mode":"legacy"}})
        );
        let prefix = format!("{header}{}", row(1, "Parent request"));
        std::fs::write(
            &parent,
            format!("{prefix}{}", row(2, "Outside the captured prefix")),
        )
        .unwrap();
        let header = format!(
            "{}\n",
            json!({"ordinal":2,"type":"session_meta","payload":{"id":"child","history_mode":"paginated","history_base":{"thread_id":"parent","end_byte_offset":prefix.len(),"end_ordinal_exclusive":2}}})
        );
        std::fs::write(&child, format!("{header}{}", row(3, "Child request"))).unwrap();
        let lookup = |id: &str| {
            assert_eq!(id, "parent");
            Ok(parent.clone())
        };
        let mut segments = lineage_from(&child, lookup).unwrap();
        let (leaf, cursor, _) = page_segments(&mut segments, None).unwrap();
        assert_eq!(cursor, prefix.len() as u64);
        assert!(leaf.last().unwrap().text.contains("Child request"));
        let (earlier, cursor, mode) = page_segments(&mut segments, Some(cursor)).unwrap();
        assert_eq!(cursor, 0);
        assert_eq!(mode, super::super::codex_history::HistoryMode::Legacy);
        assert!(earlier.last().unwrap().text.contains("Parent request"));
        assert!(earlier.iter().all(|line| !line.text.contains("Outside")));
        std::fs::write(&parent, row(9, "Replaced parent")).unwrap();
        assert!(lineage_from(&child, lookup).is_err());
    }
    #[test]
    fn every_page_uses_the_native_header_for_user_messages() {
        use super::super::codex_history::HistoryMode;
        use crate::adapter::EventKind;
        for (name, expected_mode) in [
            ("legacy", HistoryMode::Legacy),
            ("paginated", HistoryMode::Paginated),
            ("model", HistoryMode::Model),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("history.jsonl");
            let mut records = vec![json!({"type":"session_meta", "payload":{
                "id":"fixture", "history_mode":name
            }})];
            for index in 0..140 {
                let text = format!("User prompt {index}");
                records.push(json!({"type":"event_msg", "payload":{
                    "type":"user_message", "message":text
                }}));
                records.push(json!({"type":"event_msg", "payload":{
                    "type":"item_completed", "item":{
                        "type":"UserMessage", "content":[{"type":"text","text":text}]
                    }
                }}));
                records.push(json!({"type":"response_item", "payload":{
                    "type":"message", "role":"user", "content":[{
                        "type":"input_text", "text":if name == "model" { text } else {
                            format!("Model context {index}")
                        }
                    }]
                }}));
            }
            std::fs::write(
                &path,
                records
                    .iter()
                    .map(|record| format!("{record}\n"))
                    .collect::<String>(),
            )
            .unwrap();
            let redactor =
                crate::domain::redact::Redactor::new(crate::domain::redact::Persona::default());
            let mut cursor = None;
            let mut prompts = vec![];
            loop {
                let (lines, before, mode) = page(&path, cursor).unwrap();
                assert_eq!(mode, expected_mode);
                let (items, _) = super::super::supervisor::items_from_lines_with_mode(
                    "codex", &redactor, &lines, mode,
                );
                let mut earlier = items
                    .into_iter()
                    .filter(|item| item.event.kind == EventKind::UserPrompt)
                    .map(|item| item.event.text.unwrap())
                    .collect::<Vec<_>>();
                earlier.extend(prompts);
                prompts = earlier;
                if before == 0 {
                    break;
                }
                assert!(cursor.is_none_or(|end| before < end));
                cursor = Some(before);
            }
            assert_eq!(
                prompts,
                (0..140)
                    .map(|i| format!("User prompt {i}"))
                    .collect::<Vec<_>>()
            );
        }
    }
    #[test]
    fn large_attachment_records_do_not_block_earlier_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let first = "first\n";
        let large = format!("{}\n", "x".repeat(9 * 1024 * 1024));
        std::fs::write(&path, format!("{first}{large}")).unwrap();
        let (lines, before, _) = page(&path, None).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text.len(), large.len() - 1);
        let (earlier, before, _) = page(&path, Some(before)).unwrap();
        assert_eq!(earlier[0].text, "first");
        assert_eq!(before, 0);
    }
    #[test]
    fn pages_cover_each_record_once_while_new_records_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let source = (0..210)
            .map(|i| format!("{{\"index\":{i}}}\n"))
            .collect::<String>();
        std::fs::write(&path, &source).unwrap();
        let (last, mut before, _) = page(&path, None).unwrap();
        std::fs::write(&path, format!("{source}{{\"index\":210}}\n")).unwrap();
        let mut records = last;
        while before > 0 {
            let (mut earlier, next, _) = page(&path, Some(before)).unwrap();
            assert!(next < before);
            earlier.extend(records);
            records = earlier;
            before = next;
        }
        assert_eq!(records.len(), 210);
        for (index, line) in records.iter().enumerate() {
            assert_eq!(
                serde_json::from_str::<Value>(&line.text).unwrap()["index"],
                index
            );
        }
    }
}
