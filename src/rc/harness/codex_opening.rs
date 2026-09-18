//! A successful opening reply precedes executor publication and any user input.

use super::*;

const OPENING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

impl CodexDriver {
    pub(crate) async fn confirm_opening(&mut self) -> Result<(), LaunchError> {
        match tokio::time::timeout(OPENING_TIMEOUT, self.receive_opening()).await {
            Ok(result) => result,
            Err(_) => Err(LaunchError::spawned(anyhow::anyhow!(
                "Codex did not confirm its native session opening before the deadline"
            ))),
        }
    }

    async fn receive_opening(&mut self) -> Result<(), LaunchError> {
        loop {
            let (id, method) = self.handshake_request.ok_or_else(|| {
                LaunchError::spawned(anyhow::anyhow!("Codex opening has no pending request"))
            })?;
            let queued = self.proc.next().await.ok_or_else(|| {
                LaunchError::spawned(anyhow::anyhow!("Codex exited while opening its session"))
            })?;
            let value = match queued.line() {
                Line::Eof => {
                    return Err(LaunchError::spawned(anyhow::anyhow!(
                        "Codex exited before confirming {method}"
                    )));
                }
                Line::Json(value)
                    if value.get("method").is_none()
                        && value.get("id").and_then(Value::as_i64) == Some(id) =>
                {
                    value
                }
                _ => {
                    self.pushback.push(queued);
                    continue;
                }
            };
            if let Some(error) = value.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason given");
                let conflict = method == "thread/resume"
                    && error.get("code").and_then(Value::as_i64) == Some(-32600)
                    && self.resume_from.as_ref().is_some_and(|native| {
                        message == format!("thread {native} already has an active writer")
                    });
                let error = anyhow::anyhow!("Codex refused {method}: {message}");
                if conflict {
                    self.shutdown().await.map_err(|cleanup| {
                        LaunchError::spawned(anyhow::anyhow!(
                            "{error}; child shutdown is unknown: {cleanup}"
                        ))
                    })?;
                    return Err(LaunchError::external_writer(error));
                }
                return Err(LaunchError::spawned(error));
            }
            let result = value.get("result").ok_or_else(|| {
                LaunchError::spawned(anyhow::anyhow!("Codex {method} reply omitted its result"))
            })?;
            if method == "initialize" {
                self.handshake_request = None;
                self.open_thread().await.map_err(LaunchError::spawned)?;
                continue;
            }
            let native = result
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    LaunchError::spawned(anyhow::anyhow!(
                        "Codex {method} reply omitted its native session id"
                    ))
                })?;
            if self
                .resume_from
                .as_deref()
                .is_some_and(|expected| expected != native)
            {
                return Err(LaunchError::spawned(anyhow::anyhow!(
                    "Codex resume confirmed a different native session"
                )));
            }
            if result
                .pointer("/thread/canAcceptDirectInput")
                .and_then(Value::as_bool)
                == Some(false)
            {
                return Err(LaunchError::spawned(anyhow::anyhow!(
                    "Codex opened a session that cannot accept direct input"
                )));
            }
            self.thread_id = Some(native.to_owned());
            if method == "thread/start"
                && result
                    .pointer("/thread/turns")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty)
                && let Some(path) = result.pointer("/thread/path").and_then(Value::as_str)
            {
                self.fresh_history = fresh::FreshCodex::new(native, &self.cwd, PathBuf::from(path));
            }
            self.model = result
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or(self.model.take());
            self.effort = result
                .get("reasoningEffort")
                .and_then(Value::as_str)
                .map(str::to_owned);
            self.handshake_request = None;
            self.opening_ready = Some(HarnessEvent::Ready {
                runtime_thread_id: native.to_owned(),
                transcript_path: self.transcript_path(),
            });
            return Ok(());
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    async fn driver(resume: Option<&str>, responses: &[Value]) -> CodexDriver {
        let mut driver = CodexDriver::test_responder(None, responses);
        driver.resume_from = resume.map(str::to_owned);
        driver.handshake_request = Some((1, "initialize"));
        driver.next_id = Some(2);
        driver
            .send(&json!({"id": 1, "method": "initialize"}))
            .await
            .unwrap();
        driver
    }

    #[tokio::test]
    async fn opening_confirms_the_exact_reply_and_replays_ready_once() {
        let mut driver = driver(Some("native"), &[
            json!({"id":1,"result":{}}),
            json!({"method":"thread/started","params":{"threadId":"native"}}),
            json!({"id":2,"result":{"thread":{"id":"native"},"model":"fixture","reasoningEffort":"high"}}),
            json!({"method":"thread/goal/updated","params":{"threadId":"native","goal":{"text":"fixture goal"}}}),
        ]).await;
        driver.confirm_opening().await.unwrap();
        assert_eq!(driver.runtime_thread_id(), Some("native"));
        assert_eq!(driver.effort.as_deref(), Some("high"));
        assert!(
            matches!(driver.next_event().await, Some(HarnessEvent::Ready { runtime_thread_id, .. }) if runtime_thread_id == "native")
        );
        assert!(
            matches!(driver.next_event().await, Some(HarnessEvent::GoalUpdated { goal }) if goal["text"] == "fixture goal")
        );
        driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn fresh_history_is_empty_only_until_native_input_or_creator_exit() {
        let root = tempfile::tempdir().unwrap();
        for resume in [false, true] {
            let native = uuid::Uuid::new_v4().to_string();
            let mut driver = driver(
                resume.then_some(native.as_str()),
                &[
                    json!({"id":1,"result":{}}),
                    json!({"id":2,"result":{"thread":{"id":native,"turns":[],"path":root.path().join("absent.jsonl")}}}),
                    json!({"id":3,"result":{"data":[]}}),
                ],
            ).await;
            driver.cwd = root.path().to_owned();
            driver.confirm_opening().await.unwrap();
            driver.runtime_command("commands", json!({})).await.unwrap();
            let params = json!({"session_id":native,"runtime":"codex","cwd":root.path()});
            let read = crate::rc::local_history::read(params.clone());
            if resume {
                assert!(
                    read.is_err(),
                    "Resume cannot prove missing history is empty"
                );
            } else {
                let page = read.unwrap();
                assert_eq!(page["items"], json!([]));
                assert_eq!(page["status"], "complete");
                let mut invalid = params.clone();
                invalid["snapshot"] = json!("expired");
                assert!(crate::rc::local_history::read(invalid).is_err());
                assert!(matches!(
                    driver.start_turn("hello", false, None).await,
                    TurnStartDispatch::Awaiting
                ));
                assert!(crate::rc::local_history::read(params.clone()).is_err());
                let mut retained = params.clone();
                retained["snapshot"] = page["snapshot"].clone();
                assert_eq!(
                    crate::rc::local_history::read(retained).unwrap()["items"],
                    json!([])
                );
            }
            driver.shutdown().await.unwrap();
        }
        let native = uuid::Uuid::new_v4().to_string();
        let mut driver = driver(None, &[
            json!({"id":1,"result":{}}),
            json!({"id":2,"result":{"thread":{"id":native,"turns":[],"path":root.path().join("absent.jsonl")}}}),
        ]).await;
        driver.cwd = root.path().to_owned();
        driver.confirm_opening().await.unwrap();
        let params = json!({"session_id":native,"runtime":"codex","cwd":root.path()});
        assert!(crate::rc::local_history::read(params.clone()).is_ok());
        driver.shutdown().await.unwrap();
        assert!(crate::rc::local_history::read(params).is_err());
    }

    #[tokio::test]
    async fn a_notification_does_not_override_the_exact_resume_refusal() {
        let mut driver = driver(Some("native"), &[
            json!({"id":1,"result":{}}),
            json!({"method":"thread/started","params":{"threadId":"native"}}),
            json!({"id":99,"result":{"thread":{"id":"native"}}}),
            json!({"id":2,"error":{"code":-32600,"message":"thread native already has an active writer"}}),
        ]).await;
        let failure = driver.confirm_opening().await.unwrap_err();
        assert!(failure.reached_spawn());
        assert!(failure.is_external_writer());
        assert_eq!(driver.runtime_thread_id(), None);
        assert!(driver.opening_ready.is_none());
    }

    #[tokio::test]
    async fn malformed_or_mismatched_success_cannot_publish_a_session() {
        for result in [
            json!({}),
            json!({"thread":{"id":"another"}}),
            json!({"thread":{"id":"native","canAcceptDirectInput":false}}),
        ] {
            let mut driver = driver(
                Some("native"),
                &[json!({"id":1,"result":{}}), json!({"id":2,"result":result})],
            )
            .await;
            let failure = driver.confirm_opening().await.unwrap_err();
            assert!(failure.reached_spawn());
            assert!(!failure.is_external_writer());
            assert_eq!(driver.runtime_thread_id(), None);
            driver.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_different_native_error_does_not_release_the_reservation() {
        let mut driver = driver(Some("native"), &[
            json!({"id":1,"result":{}}),
            json!({"id":2,"error":{"code":-32600,"message":"thread another already has an active writer"}}),
        ]).await;
        let failure = driver.confirm_opening().await.unwrap_err();
        assert!(failure.reached_spawn());
        assert!(!failure.is_external_writer());
        driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_writer_conflict_with_uncertain_child_cleanup_keeps_the_reservation() {
        let mut driver = driver(Some("native"), &[
            json!({"id":1,"result":{}}),
            json!({"id":2,"error":{"code":-32600,"message":"thread native already has an active writer"}}),
        ]).await;
        driver.fail_test_shutdowns(1);
        let failure = driver.confirm_opening().await.unwrap_err();
        assert!(failure.reached_spawn());
        assert!(!failure.is_external_writer());
        driver.shutdown().await.unwrap();
    }
}
