//! Read-only pages addressed by native transcript byte boundaries.
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom};

pub fn read(params: Value) -> crate::Result<Value> {
    read_with_roster(params, &super::roster::Roster::try_load()?)
}

fn read_with_roster(params: Value, roster: &super::roster::Roster) -> crate::Result<Value> {
    let session = params["session_id"]
        .as_str()
        .context("Session id is required")?;
    let entry = roster.get(session);
    let starting = roster.starts.values().any(|intent| match &intent.state {
        super::roster::StartState::Pending { session: info } => info.session_id == session,
        super::roster::StartState::Completed { result } => result.session.session_id == session,
    });
    // A registered launch can be subscribed before the native thread exists.
    // Its first records arrive on that subscription after initialization.
    if entry.is_some_and(|entry| entry.thread_id.is_empty()) || (entry.is_none() && starting) {
        ensure!(
            params["before"].as_u64().unwrap_or(0) == 0,
            "Native history is not ready for paging"
        );
        return Ok(json!({"items":[],"before":0,"has_more":false,"pending":true}));
    }
    let runtime = entry
        .map(|e| e.runtime.as_str())
        .or_else(|| params["runtime"].as_str())
        .context("Runtime is required")?;
    let native = entry.map(|e| e.thread_id.as_str()).unwrap_or(session);
    ensure!(
        matches!(runtime, "codex" | "claude-code"),
        "History paging is not supported for this runtime"
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
    let adapter = crate::adapter::get(runtime)?;
    let path = adapter.resolve(native, Some(std::path::Path::new(cwd)));
    // Some runtimes materialize a registered thread only on its first turn.
    // An existing page cursor still requires its original transcript.
    if path.is_none() && starting && params["before"].is_null() {
        return Ok(json!({"items":[],"before":0,"has_more":false,"pending":true}));
    }
    let path = path.context("Native transcript is unavailable")?;
    let (lines, before, mode) = if runtime == "codex" {
        lineage_page(&path, params["before"].as_u64())?
    } else {
        page(&path, params["before"].as_u64())?
    };
    let redactor = crate::domain::redact::Redactor::try_this_machine()?;
    let (mut items, _) =
        super::supervisor::items_from_lines_with_mode(runtime, &redactor, &lines, mode);
    let items: Vec<Value> = items
        .iter_mut()
        .map(|item| {
            item.event.line = None;
            json!({"item_id":format!("history:{}",item.item_id),"event":item.event,"raw":item.raw})
        })
        .collect();
    let result = json!({"items":items,"before":before,"has_more":before>0});
    ensure!(
        serde_json::to_vec(&result)?.len() < super::local::MAX_FRAME - 1024,
        "This native history record exceeds the desktop response limit"
    );
    Ok(result)
}

struct Segment {
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
        let length = file.metadata()?.len();
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
        segments.push(Segment { file, end });
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

fn lineage_page(
    path: &std::path::Path,
    before: Option<u64>,
) -> crate::Result<(
    Vec<super::tail::TailedLine>,
    u64,
    super::codex_history::HistoryMode,
)> {
    let mut segments = lineage(path)?;
    page_segments(&mut segments, before)
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
    ensure!(
        before <= total,
        "Native history changed; reopen the conversation"
    );
    let mut base = total;
    for part in segments.iter_mut().rev() {
        base -= part.end;
        if before <= base {
            continue;
        }
        let (mut lines, next, mode) = page_file(&mut part.file, Some(before - base))?;
        for line in &mut lines {
            line.lineno += base;
        }
        return Ok((lines, base + next, mode));
    }
    Ok((vec![], 0, super::codex_history::HistoryMode::Model))
}

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
    fn registered_launch_history_waits_for_its_native_identity() {
        let session = json!({"session_id":"logical","workspace_id":"local-owner","runtime":"codex","status":"idle","last_seq":0,"created_at":"now","updated_at":"now"});
        let spec = json!({"workspace_id":"local-owner","project_id":"project","runtime":"codex","cwd":"/fixture","permission_mode":"default"});
        for state in [
            json!({"state":"pending","session":session}),
            json!({"state":"completed","result":{"session":session}}),
        ] {
            let mut roster: super::super::roster::Roster =
                serde_json::from_value(json!({"starts":{"launch":{"spec":spec,"state":state}}}))
                    .unwrap();
            let page = read_with_roster(json!({"session_id":"logical"}), &roster).unwrap();
            assert_eq!(
                page,
                json!({"items":[],"before":0,"has_more":false,"pending":true})
            );
            assert!(read_with_roster(json!({"session_id":"unknown"}), &roster).is_err());
            assert!(
                read_with_roster(json!({"session_id":"logical","before":100}), &roster).is_err()
            );
            roster.sessions.insert("logical".into(), serde_json::from_value(json!({"runtime":"codex","thread_id":"","cwd":"/fixture","workspace_id":"local-owner"})).unwrap());
            assert_eq!(
                read_with_roster(json!({"session_id":"logical"}), &roster).unwrap(),
                page
            );
            roster.sessions.get_mut("logical").unwrap().thread_id = "invalid/id".into();
            assert!(
                read_with_roster(json!({"session_id":"logical"}), &roster)
                    .unwrap_err()
                    .to_string()
                    .contains("Invalid native session id")
            );
            let entry = roster.sessions.get_mut("logical").unwrap();
            entry.runtime = "claude-code".into();
            entry.thread_id = uuid::Uuid::new_v4().to_string();
            assert_eq!(
                read_with_roster(json!({"session_id":"logical"}), &roster).unwrap(),
                page
            );
            assert!(
                read_with_roster(json!({"session_id":"logical","before":10}), &roster).is_err()
            );
            roster.starts.clear();
            assert!(read_with_roster(json!({"session_id":"logical"}), &roster).is_err());
        }
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
