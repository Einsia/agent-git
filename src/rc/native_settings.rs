//! Observed native settings do not acquire runtime control or a transcript writer.

use crate::protocol::PermissionMode;
use serde_json::{Value, json};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Clone, Default)]
pub(super) struct Settings {
    pub permission_mode: Option<PermissionMode>,
    model: Option<String>,
    effort: Option<String>,
}

impl Settings {
    pub fn model(&self) -> Value {
        json!({
            "model":self.model, "effort":self.effort,
            "effort_known":self.effort.is_some(), "settings_unknown":self.model.is_none(),
            "models":[], "efforts":[], "pending":null,
            "capabilities":{"model":false,"effort":false,"reset_model":false,"reset_effort":false}
        })
    }
}

fn bounded(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|text| !text.is_empty() && text.len() <= 512)
        .map(str::to_owned)
}

fn settings(value: &Value) -> Settings {
    let sandbox = value["sandbox_policy"]["type"].as_str();
    let approval = value["approval_policy"].as_str();
    let permission_mode = match (approval, sandbox) {
        (Some("never"), Some("danger-full-access" | "dangerFullAccess" | "disabled")) => {
            Some(PermissionMode::Bypass)
        }
        (Some("never"), Some("workspace-write" | "workspaceWrite")) => Some(PermissionMode::Auto),
        (Some("never"), Some("read-only" | "readOnly")) => Some(PermissionMode::Plan),
        (Some("on-request"), Some("workspace-write" | "workspaceWrite")) => {
            Some(PermissionMode::Default)
        }
        _ => None,
    };
    Settings {
        permission_mode,
        model: bounded(&value["model"]),
        effort: bounded(&value["effort"]),
    }
}

pub(super) fn context(line: &str) -> Option<Settings> {
    let record: Value = serde_json::from_str(line).ok()?;
    (record["type"] == "turn_context").then(|| settings(&record["payload"]))
}

pub(super) fn read_codex(path: &Path, session: &str) -> Settings {
    if let Some(observed) = indexed(path, session) {
        return observed;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return Settings::default();
    };
    let Ok(metadata) = file.metadata() else {
        return Settings::default();
    };
    let start = metadata.len().saturating_sub(4 * 1024 * 1024);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Settings::default();
    }
    let mut bytes = Vec::new();
    if file.take(4 * 1024 * 1024).read_to_end(&mut bytes).is_err() {
        return Settings::default();
    }
    let mut lines = bytes.split(|byte| *byte == b'\n');
    if start != 0 {
        lines.next();
    }
    // A partial record never replaces the last complete native observation.
    let complete = bytes.ends_with(b"\n");
    let mut rows = lines.collect::<Vec<_>>();
    if !complete {
        rows.pop();
    }
    rows.into_iter()
        .rev()
        .find_map(|line| std::str::from_utf8(line).ok().and_then(context))
        .unwrap_or_default()
}

fn indexed(path: &Path, session: &str) -> Option<Settings> {
    let index = crate::adapter::codex_index::index_path()?;
    indexed_at(&index, path, session)
}

pub(super) fn indexed_at(index: &Path, path: &Path, session: &str) -> Option<Settings> {
    let db =
        rusqlite::Connection::open_with_flags(index, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    let values: (Option<String>, Option<String>, String, String) = db.query_row(
        "SELECT substr(model,1,512),substr(reasoning_effort,1,512),substr(approval_mode,1,512),substr(sandbox_policy,1,4096) FROM threads WHERE id=?1 AND rollout_path=?2",
        rusqlite::params![session,path.to_str()?],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
    ).ok()?;
    let sandbox: Value = serde_json::from_str(&values.3).ok()?;
    Some(settings(
        &json!({"model":values.0,"effort":values.1,"approval_policy":values.2,"sandbox_policy":sandbox}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn observations_preserve_native_settings_without_granting_controls() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file,"{}",json!({"type":"turn_context","payload":{"model":"gpt-5.6-sol","effort":"max","approval_policy":"never","sandbox_policy":{"type":"danger-full-access"}}})).unwrap();
        write!(file, "{{\"type\":\"turn_context\",\"payload\":").unwrap();
        let before = std::fs::read(file.path()).unwrap();
        let observed = read_codex(file.path(), "missing");
        assert_eq!(observed.permission_mode, Some(PermissionMode::Bypass));
        assert_eq!(observed.model()["model"], "gpt-5.6-sol");
        assert_eq!(observed.model()["effort"], "max");
        assert_eq!(observed.model()["capabilities"]["model"], false);
        assert_eq!(std::fs::read(file.path()).unwrap(), before);
        assert!(
            settings(&json!({"approval_policy":"never","sandbox_policy":{"type":"unknown"}}))
                .permission_mode
                .is_none()
        );
        let index = tempfile::NamedTempFile::new().unwrap();
        let db = rusqlite::Connection::open(index.path()).unwrap();
        db.execute_batch("CREATE TABLE threads (id TEXT,rollout_path TEXT,model TEXT,reasoning_effort TEXT,approval_mode TEXT,sandbox_policy TEXT)").unwrap();
        db.execute("INSERT INTO threads VALUES (?1,?2,'selected-model','high','never','{\"type\":\"disabled\"}')", rusqlite::params!["native",file.path().to_str().unwrap()]).unwrap();
        let observed = indexed_at(index.path(), file.path(), "native").unwrap();
        assert_eq!(observed.model()["model"], "selected-model");
        assert_eq!(observed.permission_mode, Some(PermissionMode::Bypass));
        assert!(indexed_at(index.path(), file.path(), "other").is_none());
    }
}
