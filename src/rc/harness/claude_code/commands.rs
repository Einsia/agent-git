use super::*;
use anyhow::{Context, ensure};

pub(super) fn goal_from_text(text: &str) -> Option<Value> {
    if text.starts_with("No goal set") || text.starts_with("Goal cleared") {
        return Some(Value::Null);
    }
    let objective = text
        .strip_prefix("Goal set: ")
        .or_else(|| text.strip_prefix("Goal active: "))?;
    let (objective, detail) = objective
        .split_once("\nLast check: ")
        .unwrap_or((objective, ""));
    Some(json!({"objective":objective.trim(),"status":"active","detail":detail.trim()}))
}

impl ClaudeCodeDriver {
    pub async fn runtime_command(&mut self, name: &str, _arguments: Value) -> crate::Result<Value> {
        if name == "commands" {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            while self.commands.is_empty() {
                let queued = tokio::time::timeout_at(deadline, self.proc.next())
                    .await
                    .context("Claude is still initializing its command catalog")?
                    .context("Claude exited before reporting its command catalog")?;
                if let Line::Json(value) = queued.line()
                    && value["type"] == "control_response"
                    && value["response"]["response"]["commands"].is_array()
                {
                    self.classify(value.clone());
                    break;
                }
                let eof = matches!(queued.line(), Line::Eof);
                self.pushback.push(queued);
                ensure!(!eof, "Claude exited before reporting its command catalog");
            }
            return Ok(json!({"commands":self.commands}));
        }
        let command = match name {
            "goal.get" => {
                let path = self.transcript_path();
                let goal =
                    tokio::task::spawn_blocking(move || read_goal(path.as_deref())).await??;
                return Ok(json!({"goal":goal}));
            }
            "goal.clear" => "/goal clear",
            _ => anyhow::bail!("Send this Claude command through the conversation composer"),
        };
        ensure!(
            self.current_turn.is_none(),
            "Wait for Claude to finish before inspecting or clearing its goal"
        );
        ensure!(
            !self.goal_query_pending,
            "A Claude goal query is still awaiting its response"
        );
        self.goal_query_pending = true;
        self.proc.write_line(&json!({"type":"user","session_id":self.session_id,"message":{"role":"user","content":command},"parent_tool_use_id":null})).await?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let queued = tokio::time::timeout_at(deadline, self.proc.next())
                .await
                .context("Claude goal response timed out; refresh before retrying")?
                .context("Claude exited before answering the goal command")?;
            if let Line::Json(value) = queued.line() {
                if value["type"] == "result" && value["local_command"] == "goal" {
                    self.goal_query_pending = false;
                    ensure!(value["is_error"] != true, "Claude refused the goal command");
                    let goal = goal_from_text(value["result"].as_str().unwrap_or_default())
                        .context("Claude returned an unrecognized goal response")?;
                    return Ok(json!({"goal":goal}));
                }
                if value["type"] == "assistant" && value.get("local_command_source").is_some() {
                    continue;
                }
            }
            let eof = matches!(queued.line(), Line::Eof);
            self.pushback.push(queued);
            ensure!(!eof, "Claude exited before answering the goal command");
        }
    }
}

fn read_goal(path: Option<&std::path::Path>) -> crate::Result<Value> {
    use std::io::BufRead;
    let Some(path) = path else {
        return Ok(Value::Null);
    };
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Value::Null),
        Err(error) => return Err(error.into()),
    };
    let mut reader = std::io::BufReader::new(file);
    let mut bytes = Vec::new();
    let mut goal = Value::Null;
    loop {
        bytes.clear();
        if reader.read_until(b'\n', &mut bytes)? == 0 {
            break;
        }
        if !bytes.ends_with(b"\n") {
            break;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        if !text.contains("goal_status") && !text.contains("<command-name>/goal</command-name>") {
            continue;
        }
        let Ok(record) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if record["isSidechain"] == true {
            continue;
        }
        let attachment = &record["attachment"];
        if record["type"] == "attachment" && attachment["type"] == "goal_status" {
            if let Some(condition) = attachment["condition"].as_str() {
                goal = json!({"objective":condition,"status":if attachment["met"] == true {"complete"} else {"active"},"detail":attachment["reason"].as_str().unwrap_or_default()});
            }
        } else if record["type"] == "user"
            && record["message"]["content"]
                .as_str()
                .is_some_and(|text| text.contains("<command-args>clear</command-args>"))
        {
            goal = Value::Null;
        }
    }
    Ok(goal)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_goal_markers_restore_completion_without_injecting_query_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let rows=[
            json!({"type":"attachment","attachment":{"type":"goal_status","condition":"Finish","met":false}}),
            json!({"type":"attachment","isSidechain":true,"attachment":{"type":"goal_status","condition":"Another task","met":true}}),
            json!({"type":"attachment","attachment":{"type":"goal_status","condition":"Finish","met":true,"reason":"The requested output exists"}}),
        ].iter().map(|row|format!("{row}\n")).collect::<String>();
        std::fs::write(&path, &rows).unwrap();
        let result = read_goal(Some(&path)).unwrap();
        assert_eq!(result["objective"], "Finish");
        assert_eq!(result["status"], "complete");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), rows);
        std::fs::write(&path,format!("{rows}{}\n",json!({"type":"user","message":{"content":"<command-name>/goal</command-name>\n<command-args>clear</command-args>"}}))).unwrap();
        assert_eq!(read_goal(Some(&path)).unwrap(), Value::Null);
    }
}
