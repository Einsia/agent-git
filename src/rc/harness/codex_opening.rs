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
            self.model = result
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or(self.model.take());
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
            json!({"id":2,"result":{"thread":{"id":"native"},"model":"fixture"}}),
            json!({"method":"thread/goal/updated","params":{"threadId":"native","goal":{"text":"fixture goal"}}}),
        ]).await;
        driver.confirm_opening().await.unwrap();
        assert_eq!(driver.runtime_thread_id(), Some("native"));
        assert!(
            matches!(driver.next_event().await, Some(HarnessEvent::Ready { runtime_thread_id, .. }) if runtime_thread_id == "native")
        );
        assert!(
            matches!(driver.next_event().await, Some(HarnessEvent::GoalUpdated { goal }) if goal["text"] == "fixture goal")
        );
        driver.shutdown().await.unwrap();
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
