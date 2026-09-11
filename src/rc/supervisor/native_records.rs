//! Snapshot projections retain native coordinates while rows change in place.

use super::{ItemCompleted, cap_raw, projected_object_hash};
use crate::adapter::{Adapter, opencode::OpenCode};
use crate::domain::redact::Redactor;
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(crate) struct NativeRecords {
    initialized: bool,
    fingerprints: HashMap<String, String>,
}

impl NativeRecords {
    pub(crate) fn project(
        &mut self,
        bytes: &[u8],
        resuming: bool,
        redactor: &Redactor,
    ) -> crate::Result<(Vec<ItemCompleted>, Vec<String>)> {
        self.project_window(bytes, resuming, redactor, 0)
    }

    pub(crate) fn project_window(
        &mut self,
        bytes: &[u8],
        resuming: bool,
        redactor: &Redactor,
        from_line: u64,
    ) -> crate::Result<(Vec<ItemCompleted>, Vec<String>)> {
        self.project_snapshot(bytes, resuming, redactor, from_line, false)
    }

    pub(crate) fn project_final(
        &mut self,
        bytes: &[u8],
        resuming: bool,
        redactor: &Redactor,
    ) -> crate::Result<(Vec<ItemCompleted>, Vec<String>)> {
        self.project_snapshot(bytes, resuming, redactor, 0, true)
    }

    fn project_snapshot(
        &mut self,
        bytes: &[u8],
        resuming: bool,
        redactor: &Redactor,
        from_line: u64,
        finalized: bool,
    ) -> crate::Result<(Vec<ItemCompleted>, Vec<String>)> {
        let text = std::str::from_utf8(bytes)?;
        let parsed = OpenCode.parse(text)?;
        let lines = text
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<Result<Vec<_>, _>>()?;
        let completed_messages = lines
            .iter()
            .filter(|row| row["kind"] == "message")
            .filter(|row| {
                row["data"]["role"] == "user"
                    || row["data"]["time"]["completed"].is_number()
                    || row["data"]["finish"].is_string()
            })
            .filter_map(|row| row["id"].as_str())
            .collect::<HashSet<_>>();
        let unfinished_assistant = lines.iter().any(|row| {
            row["kind"] == "message"
                && row["data"]["role"] == "assistant"
                && row["id"]
                    .as_str()
                    .is_none_or(|id| !completed_messages.contains(id))
        });
        let mut current = HashMap::new();
        let mut indices = HashMap::<usize, usize>::new();
        let mut items = Vec::new();
        let mut registered = HashSet::new();
        for mut event in parsed.events {
            let Some(line) = event.line else {
                continue;
            };
            let Some(raw) = lines.get(line) else {
                anyhow::bail!("native event has no source record");
            };
            if !finalized
                && raw["kind"] == "part"
                && raw["message_id"]
                    .as_str()
                    .is_none_or(|id| !completed_messages.contains(id))
            {
                continue;
            }
            if !finalized
                && event.kind == crate::adapter::EventKind::CompactSummary
                && unfinished_assistant
            {
                continue;
            }
            let native_id = raw["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("native event record has no identity"))?;
            let index = indices.entry(line).or_default();
            let item_id = format!("opencode:{native_id}#{index}");
            *index += 1;
            // Event content can change through its host message without changing this raw row.
            let mut content = event.clone();
            content.line = None;
            let fingerprint =
                crate::domain::transcript::object_hash(&serde_json::json!([raw, content]));
            let unchanged = self.fingerprints.get(&item_id) == Some(&fingerprint);
            current.insert(item_id.clone(), fingerprint);
            if unchanged || (!self.initialized && resuming) || (line as u64) < from_line {
                continue;
            }
            let scrubbed = redactor.scrub_json(raw);
            let hash =
                projected_object_hash(raw, &scrubbed.value, !scrubbed.registered_ids.is_empty());
            registered.extend(scrubbed.registered_ids);
            let (raw, raw_truncated) = cap_raw(scrubbed.value);
            if let Some(text) = event.text.take() {
                let scrubbed = redactor.scrub(&text);
                registered.extend(scrubbed.registered_ids);
                event.text = Some(scrubbed.text);
            }
            event.paths = event
                .paths
                .iter()
                .map(|path| {
                    let scrubbed = redactor.scrub(path);
                    registered.extend(scrubbed.registered_ids);
                    scrubbed.text
                })
                .collect();
            items.push(ItemCompleted {
                item_id,
                turn_id: String::new(),
                event,
                line: line as u64,
                object_hash: hash,
                raw,
                raw_truncated,
            });
        }
        self.initialized = true;
        self.fingerprints = current;
        Ok((items, registered.into_iter().collect()))
    }
}

pub(crate) async fn read_watch_snapshot(
    source: crate::adapter::native_snapshot::Source,
    cwd: std::path::PathBuf,
) -> crate::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        let snapshot = OpenCode.snapshot_native_readonly(
            &source,
            crate::adapter::native_snapshot::Limits::default(),
        )?;
        let mut found = false;
        for line in std::str::from_utf8(&snapshot.bytes)?.lines() {
            let row: Value = serde_json::from_str(line)?;
            if row["kind"] != "opencode.meta" {
                continue;
            }
            if found || row["id"].as_str() != Some(source.session_id.as_str()) {
                anyhow::bail!("native watch metadata is ambiguous");
            }
            let directory = row["directory"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("native watch directory is missing"))?;
            if std::fs::canonicalize(directory)? != std::fs::canonicalize(&cwd)? {
                anyhow::bail!("native watch session moved outside its workspace");
            }
            found = true;
        }
        if !found {
            anyhow::bail!("native watch metadata is missing");
        }
        Ok(snapshot.bytes)
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::EventKind;
    use serde_json::json;

    fn transcript(answer: &str) -> Vec<u8> {
        [
            json!({"kind":"message","id":"user","session_id":"session","time_created":1,"data":{"role":"user"}}),
            json!({"kind":"part","id":"prompt","message_id":"user","session_id":"session","time_created":2,"data":{"type":"text","text":"hello"}}),
            json!({"kind":"message","id":"assistant","session_id":"session","time_created":3,"data":{"role":"assistant","finish":"stop"}}),
            json!({"kind":"part","id":"answer","message_id":"assistant","session_id":"session","time_created":4,"data":{"type":"text","text":answer}}),
        ].into_iter().map(|row| format!("{row}\n")).collect::<String>().into_bytes()
    }

    #[test]
    fn snapshots_keep_host_roles_native_hashes_and_replaced_row_identities() {
        let mut records = NativeRecords::default();
        let redactor = Redactor::new(crate::domain::redact::Persona {
            username: None,
            home: Some("/private-person".into()),
            hostname: None,
        });
        let bytes = transcript("/private-person/answer");
        let (items, _) = records.project(&bytes, false, &redactor).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].event.kind, EventKind::UserPrompt);
        assert_eq!(items[1].event.kind, EventKind::AssistantReply);
        assert!(
            !serde_json::to_string(&items)
                .unwrap()
                .contains("/private-person")
        );
        let raw: Value =
            serde_json::from_str(std::str::from_utf8(&bytes).unwrap().lines().nth(3).unwrap())
                .unwrap();
        assert_eq!(
            items[1].object_hash,
            crate::domain::transcript::object_hash(&raw)
        );
        assert!(
            records
                .project(&bytes, false, &redactor)
                .unwrap()
                .0
                .is_empty()
        );
        let changed = records
            .project(&transcript("replaced answer"), false, &redactor)
            .unwrap()
            .0;
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].item_id, items[1].item_id);
        assert_eq!(changed[0].event.text.as_deref(), Some("replaced answer"));
    }

    #[test]
    fn resumed_snapshots_seed_history_without_hiding_new_content() {
        let mut records = NativeRecords::default();
        let redactor = Redactor::this_machine();
        assert!(
            records
                .project(&transcript("old answer"), true, &redactor)
                .unwrap()
                .0
                .is_empty()
        );
        let items = records
            .project(&transcript("new answer"), true, &redactor)
            .unwrap()
            .0;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].event.text.as_deref(), Some("new answer"));
    }

    #[test]
    fn watch_windows_retain_host_context_and_wait_for_terminal_assistant_rows() {
        let redactor = Redactor::this_machine();
        let mut records = NativeRecords::default();
        let bytes = transcript("answer");
        let mut rows = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        rows[2]["data"].as_object_mut().unwrap().remove("finish");
        let partial = rows
            .iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>();
        assert!(
            records
                .project_window(partial.as_bytes(), false, &redactor, 3)
                .unwrap()
                .0
                .is_empty()
        );
        let items = records
            .project_window(&bytes, false, &redactor, 3)
            .unwrap()
            .0;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].event.kind, EventKind::AssistantReply);
        assert_eq!(items[0].line, 3);
    }

    #[tokio::test]
    async fn native_watches_reread_database_rows_and_reject_a_changed_workspace() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("native.sqlite");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, directory TEXT, time_created INTEGER, version TEXT);
            CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
            CREATE TABLE part(id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, time_created INTEGER, data TEXT);
            INSERT INTO message VALUES ('message','session',2,'{\"role\":\"user\"}');
            INSERT INTO part VALUES ('part','session','message',3,'{\"type\":\"text\",\"text\":\"before\"}');").unwrap();
        db.execute(
            "INSERT INTO session VALUES ('session','project',NULL,?1,1,'test')",
            [root.path().to_str().unwrap()],
        )
        .unwrap();
        let source = crate::adapter::native_snapshot::Source {
            runtime: "opencode",
            session_id: "session".into(),
            path,
            database: true,
        };
        let first = read_watch_snapshot(source.clone(), root.path().to_path_buf())
            .await
            .unwrap();
        db.execute(
            "UPDATE part SET data = ?1",
            [r#"{"type":"text","text":"after"}"#],
        )
        .unwrap();
        let next = read_watch_snapshot(source.clone(), root.path().to_path_buf())
            .await
            .unwrap();
        assert_ne!(first, next);
        assert!(
            OpenCode
                .parse(std::str::from_utf8(&next).unwrap())
                .unwrap()
                .events
                .iter()
                .any(|event| event.text.as_deref() == Some("after"))
        );
        let foreign = tempfile::tempdir().unwrap();
        db.execute(
            "UPDATE session SET directory = ?1",
            [foreign.path().to_str().unwrap()],
        )
        .unwrap();
        assert!(
            read_watch_snapshot(source, root.path().to_path_buf())
                .await
                .is_err()
        );
    }
}
