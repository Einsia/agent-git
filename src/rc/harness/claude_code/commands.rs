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
        match name {
            "goal.get" => {
                let path = self.transcript_path();
                let goal =
                    tokio::task::spawn_blocking(move || read_goal(path.as_deref())).await??;
                Ok(json!({"goal":goal}))
            }
            "goal.clear" => self.clear_goal(std::time::Duration::from_secs(10)).await,
            _ => anyhow::bail!("Send this Claude command through the conversation composer"),
        }
    }

    async fn clear_goal(&mut self, timeout: std::time::Duration) -> crate::Result<Value> {
        ensure!(
            self.current_turn.is_none()
                && !self.interrupt_draining
                && self.awaiting_echoes.is_empty(),
            "Wait for Claude to finish before clearing its goal"
        );
        ensure!(
            !self.goal_query_pending,
            "A Claude goal query is still awaiting its response"
        );
        self.goal_query_pending = true;
        let mut completed = false;
        let result = tokio::time::timeout(timeout, async {
            self.proc.write_line(&json!({"type":"user","session_id":self.session_id,"message":{"role":"user","content":"/goal clear"},"parent_tool_use_id":null})).await?;
            loop {
                let queued = self.proc.next().await.context("Claude exited before answering the goal command")?;
                if let Line::Json(value) = queued.line() {
                    if goal_result(value, &self.session_id) {
                        completed = true;
                        ensure!(value["is_error"] != true, "Claude refused the goal command");
                        let goal = goal_from_text(value["result"].as_str().unwrap_or_default())
                            .context("Claude returned an unrecognized goal response")?;
                        return Ok(json!({"goal":goal}));
                    }
                    if (value["type"] == "assistant" && value.get("local_command_source").is_some())
                        || (value["type"] == "user" && value["message"]["content"] == "/goal clear") {
                        continue;
                    }
                }
                let eof = matches!(queued.line(), Line::Eof);
                self.pushback.push(queued);
                ensure!(!eof, "Claude exited before answering the goal command");
            }
        }).await.context("Claude goal response timed out")
            .and_then(|result| result);
        if !completed {
            // An uncorrelated late result must not close a subsequent conversation turn.
            self.proc.shutdown().await.context(
                "Claude goal recovery could not stop the runtime; restart the local daemon",
            )?;
        }
        self.goal_query_pending = false;
        if completed {
            result
        } else {
            result.context("Resume this session to reconnect before retrying the goal command")
        }
    }
}

fn goal_result(value: &Value, session: &str) -> bool {
    value["type"] == "result"
        && value["session_id"].as_str().is_none_or(|id| id == session)
        && (value["local_command"] == "goal"
            || (value["subtype"] == "success"
                && value["is_error"] != true
                && goal_from_text(value["result"].as_str().unwrap_or_default())
                    == Some(Value::Null)))
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
    #[cfg(unix)]
    #[tokio::test]
    async fn goal_clear_accepts_tagged_and_unmarked_results_without_phantom_turns() {
        for marked in [false, true] {
            let mut driver = ClaudeCodeDriver::test_driver();
            driver.clear_test_current_turn();
            driver.shutdown().await.unwrap();
            let mut response = json!({"type":"result", "subtype":"success", "is_error":false, "result":"No goal set"});
            if marked {
                response["local_command"] = json!("goal");
            }
            driver.proc = Proc::spawn(
                "sh",
                &[
                    "-c".into(),
                    "while IFS= read -r input; do printf '%s\\n' \"$input\" \"$1\"; done".into(),
                    "goal-fixture".into(),
                    response.to_string(),
                ],
                &PathBuf::from("/"),
                &[],
            )
            .unwrap();
            for _ in 0..2 {
                assert_eq!(
                    driver
                        .runtime_command("goal.clear", Value::Null)
                        .await
                        .unwrap(),
                    json!({"goal":null})
                );
                assert!(!driver.goal_query_pending);
                assert_eq!(driver.pushback.len(), 0);
            }
            driver.shutdown().await.unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn goal_clear_timeout_and_write_failure_have_a_reconnect_path() {
        let mut driver = ClaudeCodeDriver::test_driver();
        driver.clear_test_current_turn();
        let error = driver
            .clear_goal(std::time::Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("timed out"));
        assert!(error.to_string().contains("Resume this session"));
        assert!(!driver.goal_query_pending);
        assert!(matches!(
            driver.next_event().await,
            Some(HarnessEvent::Exited { .. })
        ));
        let error = driver
            .runtime_command("goal.clear", Value::Null)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Resume this session"));
        assert!(!driver.goal_query_pending);
    }

    #[test]
    fn goal_results_reject_foreign_sessions_and_ordinary_turn_output() {
        assert!(!goal_result(
            &json!({"type":"result", "local_command":"goal", "session_id":"foreign"}),
            "current"
        ));
        assert!(!goal_result(
            &json!({"type":"result", "subtype":"success", "result":"ordinary output"}),
            "current"
        ));
        assert!(!goal_result(
            &json!({"type":"result", "subtype":"success", "is_error":true, "result":"No goal set"}),
            "current"
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn goal_recovery_refuses_new_turns_until_the_process_is_stopped() {
        let mut driver = ClaudeCodeDriver::test_driver();
        driver.clear_test_current_turn();
        driver.fail_test_shutdowns(1);
        assert!(
            driver
                .clear_goal(std::time::Duration::from_millis(100))
                .await
                .is_err()
        );
        assert!(driver.goal_query_pending);
        assert!(matches!(
            driver.start_turn("new turn").await,
            super::super::super::TurnStartOutcome::RetryableNotAccepted { .. }
        ));
        driver.shutdown().await.unwrap();
    }

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
