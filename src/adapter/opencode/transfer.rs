//! V2 transfers carry whole native messages rather than classic message/part exports.

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::HashMap, path::Path, process::Command};

pub(super) fn is_v2(cli: &Path) -> Result<bool> {
    let output = Command::new(cli)
        .arg("--version")
        .output()
        .context("cannot inspect OpenCode import protocol")?;
    ensure!(
        output.status.success(),
        "cannot inspect OpenCode import protocol"
    );
    let text = String::from_utf8(output.stdout)?;
    let version = text
        .trim()
        .strip_prefix("opencode ")
        .unwrap_or(text.trim())
        .trim_start_matches('v');
    let major: u32 = version
        .split('.')
        .next()
        .context("OpenCode version is missing")?
        .parse()
        .context("OpenCode version is unrecognized")?;
    Ok(major >= 2)
}

pub(super) fn payload(content: &str, new_id: &str, cwd: &Path) -> Result<String> {
    if !super::canonical_or_export(content) {
        let legacy: Value = serde_json::from_str(content)?;
        return serde_json::to_string_pretty(&from_classic(legacy, new_id, cwd)?)
            .map_err(Into::into);
    }
    let rows = content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str::<Value>)
        .collect::<serde_json::Result<Vec<_>>>()?;
    if rows
        .iter()
        .any(|row| matches!(row["kind"].as_str(), Some("message" | "part")))
    {
        ensure!(
            !rows.iter().any(|row| row["kind"] == super::PROJECTED),
            "cannot restore mixed OpenCode storage generations"
        );
        return serde_json::to_string_pretty(&from_classic(
            super::canonical_to_export(content, new_id, cwd)?,
            new_id,
            cwd,
        )?)
        .map_err(Into::into);
    }
    let meta = rows
        .first()
        .filter(|row| row["kind"] == "opencode.meta")
        .context("OpenCode metadata is missing")?;
    let mut ids = HashMap::new();
    ids.insert(
        meta["id"]
            .as_str()
            .context("OpenCode session ID is missing")?
            .to_owned(),
        new_id.to_owned(),
    );
    for row in rows.iter().skip(1) {
        ensure!(
            row["kind"] == super::PROJECTED,
            "unsupported OpenCode transfer record"
        );
        let id = row["id"]
            .as_str()
            .context("OpenCode message ID is missing")?;
        ensure!(
            ids.insert(id.to_owned(), super::mint_key("msg_")).is_none(),
            "duplicate OpenCode message identity"
        );
    }
    let mut messages = Vec::new();
    for row in rows.iter().skip(1) {
        let mut data = row["data"].clone();
        let fields = data
            .as_object_mut()
            .context("OpenCode message data is not an object")?;
        // Native text, tool inputs and provider payloads are content, not identity references.
        for key in ["sessionID", "parentID", "messageID", "tail_start_id"] {
            if let Some(replacement) = fields
                .get(key)
                .and_then(Value::as_str)
                .and_then(|id| ids.get(id))
            {
                fields.insert(key.into(), replacement.clone().into());
            }
        }
        fields.insert("id".into(), ids[row["id"].as_str().unwrap()].clone().into());
        fields.insert("type".into(), row["type"].clone());
        fields
            .entry("time")
            .or_insert_with(|| json!({"created":row["time_created"]}));
        ensure_settled(&data)?;
        messages.push(data);
    }
    let result = transfer(
        new_id,
        cwd,
        meta["time_created"].as_i64().unwrap_or(0),
        messages,
    );
    Ok(serde_json::to_string_pretty(&result)?)
}

fn ensure_settled(message: &Value) -> Result<()> {
    ensure!(
        message["type"] != "assistant"
            || message
                .pointer("/time/completed")
                .is_some_and(Value::is_number),
        "cannot import an unfinished OpenCode assistant message"
    );
    ensure!(
        !matches!(message["type"].as_str(), Some("shell" | "compaction"))
            || message["status"] != "running",
        "cannot import unfinished OpenCode work"
    );
    for part in message["content"].as_array().into_iter().flatten() {
        ensure!(
            part["type"] != "tool"
                || matches!(
                    part["state"]["status"].as_str(),
                    Some("completed" | "error")
                ),
            "cannot import an unfinished OpenCode tool"
        );
    }
    Ok(())
}

fn transfer(id: &str, cwd: &Path, created: i64, messages: Vec<Value>) -> Value {
    let updated = messages
        .iter()
        .filter_map(|m| m.pointer("/time/created").and_then(Value::as_i64))
        .max()
        .unwrap_or(created)
        .max(created);
    let title = messages
        .iter()
        .find(|m| m["type"] == "user")
        .and_then(|m| m["text"].as_str())
        .unwrap_or("Imported session");
    json!({
        "info": {"id":id,"projectID":"global","location":{"directory":cwd},"title":title.chars().take(120).collect::<String>(),
            "cost":0,"tokens":{"input":0,"output":0,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":created,"updated":updated}},
        "messages":messages
    })
}

/// Classic transfers are converted explicitly; their original fields remain archival metadata.
fn from_classic(legacy: Value, id: &str, cwd: &Path) -> Result<Value> {
    let rows = legacy["messages"]
        .as_array()
        .context("OpenCode export has no messages")?;
    let mut messages = Vec::new();
    for row in rows {
        let info = &row["info"];
        let parts = row["parts"]
            .as_array()
            .context("OpenCode message has no parts")?;
        let created = info
            .pointer("/time/created")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let message_id = info["id"].as_str().context("OpenCode message has no ID")?;
        let texts = || {
            parts
                .iter()
                .filter(|p| p["type"] == "text")
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        };
        if info["role"] == "user" {
            if parts.iter().any(|p| p["type"] == "compaction") {
                continue;
            }
            let files = parts
                .iter()
                .filter(|p| p["type"] == "file")
                .map(file_attachment)
                .collect::<Result<Vec<_>>>()?;
            messages.push(json!({"id":message_id,"type":"user","time":{"created":created},"text":texts(),"files":files,"metadata":{"agit_classic":row}}));
            continue;
        }
        ensure!(
            info["role"] == "assistant",
            "unsupported classic OpenCode message role"
        );
        let completed = info
            .pointer("/time/completed")
            .and_then(Value::as_i64)
            .context("cannot import an unfinished OpenCode assistant message")?;
        if info["summary"] == true {
            messages.push(json!({"id":message_id,"type":"compaction","time":{"created":created},"status":"completed","reason":"auto","summary":texts(),"metadata":{"agit_classic":row}}));
            continue;
        }
        let mut content = Vec::new();
        for part in parts {
            match part["type"].as_str() {
                Some("text" | "reasoning") => {
                    content.push(json!({"type":part["type"],"text":part["text"]}))
                }
                Some("tool") => {
                    let state = &part["state"];
                    ensure!(
                        matches!(state["status"].as_str(), Some("completed" | "error")),
                        "cannot import an unfinished OpenCode tool"
                    );
                    let start = state
                        .pointer("/time/start")
                        .and_then(Value::as_i64)
                        .unwrap_or(created);
                    let end = state
                        .pointer("/time/end")
                        .and_then(Value::as_i64)
                        .unwrap_or(start);
                    let mut results =
                        vec![json!({"type":"text","text":state["output"].as_str().unwrap_or("")})];
                    for file in state["attachments"].as_array().into_iter().flatten() {
                        results.push(json!({"type":"file","uri":file["url"],"mime":file["mime"],"name":file["filename"]}));
                    }
                    let mut projected =
                        json!({"status":state["status"],"input":state["input"],"content":results});
                    if state["metadata"].is_object() {
                        projected["metadata"] = state["metadata"].clone();
                    }
                    if state["status"] == "error" {
                        projected["error"] = json!({"type":"ToolError","message":state["error"].as_str().unwrap_or("Tool failed")});
                    }
                    content.push(json!({"type":"tool","id":part["callID"],"name":part["tool"],"state":projected,"time":{"created":start,"ran":start,"completed":end}}));
                }
                _ => {}
            }
        }
        messages.push(json!({"id":message_id,"type":"assistant","time":{"created":created,"completed":completed},
            "agent":info["agent"].as_str().unwrap_or("build"),"model":{"id":info["modelID"].as_str().unwrap_or("unknown"),"providerID":info["providerID"].as_str().unwrap_or("agit")},
            "content":content,"metadata":{"agit_classic":row}}));
    }
    Ok(transfer(
        id,
        cwd,
        legacy["info"]["time"]["created"].as_i64().unwrap_or(0),
        messages,
    ))
}

fn file_attachment(part: &Value) -> Result<Value> {
    let uri = part["url"].as_str().context("OpenCode file has no URL")?;
    let mime = part["mime"]
        .as_str()
        .context("OpenCode file has no MIME type")?;
    let (data, source) = if let Some(inline) = uri.strip_prefix("data:") {
        let (header, data) = inline
            .split_once(',')
            .context("invalid OpenCode data URL")?;
        ensure!(
            header.ends_with(";base64"),
            "OpenCode file data URL must be base64 encoded"
        );
        (data, json!({"type":"inline"}))
    } else {
        ("", json!({"type":"uri","uri":uri}))
    };
    let mut file = json!({"data":data,"mime":mime,"source":source});
    if let Some(name) = part["filename"].as_str() {
        file["name"] = name.into();
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical() -> String {
        let rows = [
            json!({"kind":"opencode.meta","id":"ses_source","time_created":1}),
            json!({"kind":"session_message","id":"msg_user","type":"user","time_created":2,"data":{"time":{"created":2},"text":"Synthetic transfer probe"}}),
            json!({"kind":"session_message","id":"msg_assistant","type":"assistant","time_created":3,"data":{
                "time":{"created":3,"completed":4},"agent":"build","model":{"id":"test","providerID":"test"},
                "content":[{"type":"tool","id":"call_probe","name":"read","time":{"created":3,"completed":4},"state":{"status":"completed","input":{"path":"fixture.txt"},"content":[{"type":"text","text":"Synthetic tool output"}]}},{"type":"text","text":"Synthetic reply"}]}}),
            json!({"kind":"session_message","id":"msg_summary","type":"compaction","time_created":5,"data":{"time":{"created":5},"status":"completed","reason":"manual","summary":"Synthetic summary"}}),
        ];
        rows.iter().map(|row| format!("{row}\n")).collect()
    }

    /// A V2 round trip retains native tool outputs and compaction rather than projecting through classic parts.
    #[test]
    fn native_transfer_preserves_content_and_refuses_unfinished_work() {
        let original = canonical();
        let encoded: Value = serde_json::from_str(
            &payload(&original, "ses_destination", Path::new("/target")).unwrap(),
        )
        .unwrap();
        assert_eq!(encoded["info"]["id"], "ses_destination");
        assert_eq!(
            encoded["messages"][1]["content"][0]["state"]["content"][0]["text"],
            "Synthetic tool output"
        );
        assert_eq!(encoded["messages"][2]["summary"], "Synthetic summary");
        assert_ne!(encoded["messages"][0]["id"], "msg_user");
        let literal_id: Value = serde_json::from_str(
            &payload(
                &original.replace("Synthetic transfer probe", "msg_user"),
                "ses_destination",
                Path::new("/target"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(literal_id["messages"][0]["text"], "msg_user");
        assert!(
            payload(
                &original.replace("\"status\":\"completed\"", "\"status\":\"running\""),
                "ses_destination",
                Path::new("/target")
            )
            .is_err()
        );
        assert!(
            payload(
                &original.replace("\"completed\":4", "\"incomplete\":4"),
                "ses_destination",
                Path::new("/target")
            )
            .is_err()
        );
    }

    /// The installed CLI must accept the transfer and expose it to agit's own database reader.
    #[test]
    #[ignore = "requires OPENCODE_TEST_BINARY pointing to an installed V2 CLI"]
    fn installed_v2_import_is_discoverable_and_preserves_native_context() {
        let cli = std::env::var_os("OPENCODE_TEST_BINARY").expect("OPENCODE_TEST_BINARY");
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let database = cwd.join("probe.db");
        let id = "ses_agittransferprobe";
        let file = cwd.join("transfer.json");
        std::fs::write(&file, payload(&canonical(), id, &cwd).unwrap()).unwrap();
        let mut cmd = Command::new(cli);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("OPENCODE_") {
                cmd.env_remove(key);
            }
        }
        for (key, name) in [
            ("HOME", "home"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
        ] {
            let path = cwd.join(name);
            std::fs::create_dir_all(&path).unwrap();
            cmd.env(key, path);
        }
        cmd.env("OPENCODE_DB", &database);
        let mut retry = Command::new(cmd.get_program());
        for (key, value) in cmd.get_envs() {
            if let Some(value) = value {
                retry.env(key, value);
            } else {
                retry.env_remove(key);
            }
        }
        super::super::run_import(&mut cmd, &file, &cwd, true).unwrap();
        let con = super::super::open(&database).unwrap();
        let listed = super::super::list_for_repo(&con, cwd.to_str().unwrap()).unwrap();
        assert!(listed.iter().any(|session| session.id == id));
        let text = super::super::materialize(&con, id).unwrap().text;
        assert!(text.contains("Synthetic tool output"));
        assert!(text.contains("Synthetic summary"));
        use crate::adapter::Adapter;
        let session = super::super::OpenCode.parse(&text).unwrap();
        assert_eq!(session.counts().prompts, 1);
        assert_eq!(session.counts().tools, 1);
        assert_eq!(session.counts().compactions, 1);
        // Some installed versions accept an identical retry; others report an identity conflict.
        let _ = super::super::run_import(&mut retry, &file, &cwd, true);
        assert_eq!(super::super::materialize(&con, id).unwrap().text, text);

        let classic_id = "ses_agitclassicprobe";
        let mut classic =
            super::super::canonical_to_export(&canonical(), classic_id, &cwd).unwrap();
        classic["messages"][0]["parts"].as_array_mut().unwrap().push(json!({"type":"file","url":"data:text/plain;base64,cHJvYmU=","mime":"text/plain","filename":"probe.txt"}));
        let tool = classic["messages"][1]["parts"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|p| p["type"] == "tool")
            .unwrap();
        tool["state"]["status"] = "error".into();
        tool["state"]["error"] = "Synthetic failure".into();
        tool["state"]["attachments"] = json!([{"url":"data:text/plain;base64,cHJvYmU=","mime":"text/plain","filename":"result.txt"}]);
        let mut unfinished = classic.clone();
        unfinished["messages"][1]["info"]["time"]
            .as_object_mut()
            .unwrap()
            .remove("completed");
        assert!(payload(&unfinished.to_string(), classic_id, &cwd).is_err());
        std::fs::write(
            &file,
            payload(&classic.to_string(), classic_id, &cwd).unwrap(),
        )
        .unwrap();
        let mut classic_cmd = Command::new(cmd.get_program());
        for (key, value) in cmd.get_envs() {
            if let Some(value) = value {
                classic_cmd.env(key, value);
            } else {
                classic_cmd.env_remove(key);
            }
        }
        super::super::run_import(&mut classic_cmd, &file, &cwd, true).unwrap();
        let text = super::super::materialize(&con, classic_id).unwrap().text;
        let session = super::super::OpenCode.parse(&text).unwrap();
        assert_eq!(session.counts().prompts, 1);
        assert_eq!(session.counts().tools, 1);
        assert_eq!(session.counts().compactions, 1);
        assert!(text.contains("Synthetic tool output"));
        assert!(text.contains("Synthetic failure"));
        let rows: Vec<Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let user = rows.iter().find(|row| row["type"] == "user").unwrap();
        assert_eq!(user["data"]["files"][0]["data"], "cHJvYmU=");
        assert_eq!(user["data"]["files"][0]["source"]["type"], "inline");
        let assistant = rows.iter().find(|row| row["type"] == "assistant").unwrap();
        let tool = assistant["data"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|part| part["type"] == "tool")
            .unwrap();
        assert_eq!(tool["state"]["status"], "error");
        assert_eq!(tool["state"]["content"][1]["name"], "result.txt");

        let (_source_dir, source_db) = super::super::tests::media_fixture_db();
        let source = super::super::open(&source_db).unwrap();
        let media = super::super::materialize(&source, "ses_v2").unwrap().text;
        let media_id = "ses_agitmediaprobe";
        std::fs::write(&file, payload(&media, media_id, &cwd).unwrap()).unwrap();
        let mut media_cmd = Command::new(cmd.get_program());
        for (key, value) in cmd.get_envs() {
            if let Some(value) = value {
                media_cmd.env(key, value);
            } else {
                media_cmd.env_remove(key);
            }
        }
        super::super::run_import(&mut media_cmd, &file, &cwd, true).unwrap();
        let restored = super::super::materialize(&con, media_id).unwrap().text;
        assert!(!restored.contains("media:"));
        assert_eq!(
            restored.matches("data:text/plain;base64,cHJvYmU=").count(),
            2
        );
        let rows: Vec<Value> = restored
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let user = rows.iter().find(|row| row["type"] == "user").unwrap();
        assert_eq!(user["data"]["files"][0]["data"], "cHJvYmU=");
    }
}
