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
                Line::Fatal(message) => {
                    return Err(LaunchError::spawned(anyhow::anyhow!(
                        "Codex opening transport failed: {message}"
                    )));
                }
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
                if method == "thread/resume" {
                    self.shutdown().await.map_err(|cleanup| {
                        LaunchError::spawned(anyhow::anyhow!(
                            "{error}; child shutdown is unknown: {cleanup}"
                        ))
                    })?;
                    return Err(if conflict {
                        LaunchError::external_writer(error)
                    } else {
                        LaunchError::resume_rejected(error)
                    });
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
            if method == "thread/start" {
                // RC publishes a durable session before its first input. A native metadata
                // write materializes lazy history; replaying its Git SHA preserves both
                // the conversation and its title without synthesizing transcript records.
                let persisted = self
                    .command_request(
                        "thread/metadata/update",
                        json!({"threadId":native,"gitInfo":{"sha":result.pointer("/thread/gitInfo/sha")}}),
                    )
                    .await
                    .map_err(LaunchError::spawned)?;
                if persisted.pointer("/thread/id").and_then(Value::as_str) != Some(native) {
                    return Err(LaunchError::spawned(anyhow::anyhow!(
                        "Codex did not confirm persistence of the new session"
                    )));
                }
            }
            self.thread_id = Some(native.to_owned());
            self.model = result
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or(self.model.take());
            self.effort = result
                .get("reasoningEffort")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if let Some(mode) = crate::rc::native_settings::native_reply(result).permission_mode {
                self.mode = mode;
                self.native_mode_unknown = false;
            } else if self.proc.shared() {
                self.native_mode_unknown = true;
            }
            if self.proc.shared() {
                self.current_turn = shared_active_turn(result).map_err(LaunchError::spawned)?;
            }
            self.handshake_request = None;
            self.opening_ready = Some(HarnessEvent::Ready {
                runtime_thread_id: native.to_owned(),
                transcript_path: self.transcript_path(),
            });
            self.control_acquired();
            return Ok(());
        }
    }
}

pub(super) fn shared_active_turn(result: &Value) -> crate::Result<Option<String>> {
    let turns = result
        .pointer("/initialTurnsPage/data")
        .or_else(|| result.pointer("/thread/turns"))
        .and_then(Value::as_array);
    let mut active = None;
    for turn in turns
        .into_iter()
        .flatten()
        .filter(|turn| turn["status"] == "inProgress")
    {
        let id = turn["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Codex resume omitted its active turn id"))?;
        validate_native_turn_id(id).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            active.is_none(),
            "Codex resume reported multiple active turns"
        );
        active = Some(id.to_owned());
    }
    anyhow::ensure!(
        result
            .pointer("/thread/status/type")
            .and_then(Value::as_str)
            != Some("active")
            || active.is_some(),
        "Codex resume reported an active session without its current turn; reconnect with a Codex version supporting bounded turn metadata"
    );
    Ok(active)
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

    #[test]
    fn bounded_resume_preserves_active_turn_and_rejects_missing_identity() {
        let active = json!({"thread":{"status":{"type":"active"},"turns":[]},
            "initialTurnsPage":{"data":[{"id":"active-turn","status":"inProgress","items":[]}]}});
        assert_eq!(
            shared_active_turn(&active).unwrap().as_deref(),
            Some("active-turn")
        );
        let mut missing = active.clone();
        missing["initialTurnsPage"]["data"] = json!([]);
        assert!(shared_active_turn(&missing).is_err());
        let idle = json!({"thread":{"status":{"type":"idle"},"turns":[]},
            "initialTurnsPage":{"data":[{"id":"done","status":"completed","items":[]}]}});
        assert_eq!(shared_active_turn(&idle).unwrap(), None);
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
    async fn new_opening_requires_native_persistence_before_publication() {
        for reply in [
            json!({"id":3,"result":{"thread":{"id":"native"}}}),
            json!({"id":3,"error":{"code":-32603,"message":"storage unavailable"}}),
            json!({"id":3,"result":{"thread":{"id":"other"}}}),
        ] {
            let root = tempfile::tempdir().unwrap();
            let capture = root.path().join("opening.jsonl");
            let mut driver = CodexDriver::test_responder(None, &[]);
            driver.shutdown().await.unwrap();
            driver.proc = Proc::spawn(
                "sh",
                &[
                    "-c".into(),
                    concat!(
                        "IFS= read -r request\n",
                        "printf '%s\\n' '{\"id\":1,\"result\":{}}'\n",
                        "IFS= read -r request\n",
                        "printf '%s\\n' \"$request\" > \"$AGIT_OPENING_CAPTURE\"\n",
                        "printf '%s\\n' \"$AGIT_OPENING_RESULT\"\n",
                        "IFS= read -r request\n",
                        "printf '%s\\n' \"$request\" >> \"$AGIT_OPENING_CAPTURE\"\n",
                        "printf '%s\\n' \"$AGIT_PERSIST_RESULT\"\n",
                        "while IFS= read -r request; do :; done\n"
                    )
                    .into(),
                ],
                &root.path().to_path_buf(),
                &[
                    ("AGIT_OPENING_CAPTURE".into(), capture.to_string_lossy().into()),
                    ("AGIT_OPENING_RESULT".into(), json!({"id":2,"result":{"thread":{"id":"native","gitInfo":{"sha":"existing-sha"}},"model":"fixture"}}).to_string()),
                    ("AGIT_PERSIST_RESULT".into(), reply.to_string()),
                ],
            )
            .unwrap().into();
            driver.handshake_request = Some((1, "initialize"));
            driver.next_id = Some(2);
            driver
                .send(&json!({"id":1,"method":"initialize"}))
                .await
                .unwrap();
            let result = driver.confirm_opening().await;
            let requests: Vec<Value> = std::fs::read_to_string(capture)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(requests[0]["params"]["historyMode"], "legacy");
            assert_eq!(requests[0]["params"]["ephemeral"], false);
            assert_eq!(
                requests[1],
                json!({"id":3,"method":"thread/metadata/update","params":{"threadId":"native","gitInfo":{"sha":"existing-sha"}}})
            );
            if reply.pointer("/result/thread/id").and_then(Value::as_str) == Some("native") {
                result.unwrap();
                assert_eq!(driver.runtime_thread_id(), Some("native"));
                assert!(matches!(
                    driver.opening_ready,
                    Some(HarnessEvent::Ready { .. })
                ));
            } else {
                assert!(result.unwrap_err().reached_spawn());
                assert_eq!(driver.runtime_thread_id(), None);
                assert!(driver.opening_ready.is_none());
            }
            driver.shutdown().await.unwrap();
        }
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
    async fn an_explicit_resume_refusal_releases_only_its_opening_reservation() {
        let mut driver = driver(Some("native"), &[
            json!({"id":1,"result":{}}),
            json!({"id":2,"error":{"code":-32600,"message":"failed to load configuration: Model provider `custom` not found"}}),
        ]).await;
        let failure = driver.confirm_opening().await.unwrap_err();
        assert!(failure.reached_spawn());
        assert!(!failure.is_external_writer());
        assert!(failure.is_resume_rejected());
        assert_eq!(driver.runtime_thread_id(), None);
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
        assert!(!failure.is_resume_rejected());
        driver.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn shared_resume_preserves_settings_and_filters_other_threads() {
        for approval in ["never", "on-request"] {
            shared_resume_with_policy(approval).await;
        }
    }

    async fn shared_resume_with_policy(approval: &'static str) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("codex.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut connection = tokio_tungstenite::accept_async(stream).await.unwrap();
            let request: Value =
                serde_json::from_str(connection.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert_eq!(request["method"], "initialize");
            connection
                .send(Message::Text(
                    json!({"id":request["id"],"result":{}}).to_string().into(),
                ))
                .await
                .unwrap();
            let request: Value =
                serde_json::from_str(connection.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert_eq!(request["method"], "thread/resume");
            assert_eq!(
                request["params"],
                json!({"threadId":"native","excludeTurns":true,
                "initialTurnsPage":{"limit":1,"itemsView":"notLoaded","sortDirection":"desc"}})
            );
            connection.send(Message::Text(json!({"id":request["id"],"result":{"thread":{"id":"native"},"model":"native-model","approvalPolicy":approval,"sandbox":{"type":"readOnly"}}}).to_string().into())).await.unwrap();
            let request: Value =
                serde_json::from_str(connection.next().await.unwrap().unwrap().to_text().unwrap())
                    .unwrap();
            assert_eq!(request["method"], "thread/settings/update");
            assert_eq!(
                request["params"],
                json!({"threadId":"native","model":"native-model","effort":"high"})
            );
            connection
                .send(Message::Text(
                    json!({"id":request["id"],"result":{}}).to_string().into(),
                ))
                .await
                .unwrap();
            while let Some(frame) = connection.next().await {
                if matches!(frame, Ok(Message::Close(_))) {
                    break;
                }
            }
        });
        let source = crate::rc::native_codex::Source::new(
            root.path(),
            &std::env::current_exe().unwrap(),
            Some(&socket),
        )
        .unwrap();
        let mut driver = CodexDriver::launch_shared(
            LaunchSpec {
                cwd: root.path().to_path_buf(),
                resume_from: Some("native".into()),
                agit_session: None,
                model: Some("must-not-override".into()),
                dangerous: true,
                permission_mode: Some(PermissionMode::Bypass),
            },
            &source,
        )
        .await
        .unwrap();
        driver.confirm_opening().await.unwrap();
        driver.expire_test_fallback_control();
        assert!(driver.background_after_silence(true).await.is_none());
        assert_eq!(driver.control_snapshot()["mode"], "shared");
        assert_eq!(driver.model.as_deref(), Some("native-model"));
        assert_eq!(driver.permission_mode_known(), approval == "never");
        if driver.permission_mode_known() {
            assert_eq!(driver.permission_mode(), PermissionMode::Plan);
        }
        driver.model_catalog =
            vec![json!({"id":"native-model","efforts":[{"id":"high"}],"default_effort":"high"})];
        driver.current_turn = Some("active-turn".into());
        let patched = driver
            .model_control(Some(&crate::rc::harness::models::ModelPatch {
                model: None,
                effort: Some(Some("high".into())),
            }))
            .await
            .unwrap();
        assert_eq!(patched["model"], "native-model");
        assert_eq!(patched["effort"], "high");
        assert_eq!(patched["applied"], "next_turn");
        assert!(driver.pending_model.is_none());
        assert_eq!(driver.current_turn.take().as_deref(), Some("active-turn"));
        for event in [
            json!({"method":"thread/started","params":{"thread":{"id":"other"}}}),
            json!({"method":"turn/started","params":{"threadId":"other","turn":{"id":"other-turn"}}}),
            json!({"id":99,"method":"item/commandExecution/requestApproval","params":{"threadId":"other","turnId":"other-turn","itemId":"command"}}),
        ] {
            assert!(driver.classify(event).await.is_none());
        }
        assert_eq!(driver.thread_id.as_deref(), Some("native"));
        assert!(driver.current_turn.is_none());
        assert!(driver.pending_approvals.is_empty());
        driver.shutdown().await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires AGIT_NATIVE_CODEX_TEST_BINARY and starts an isolated native daemon"]
    async fn native_shared_server_survives_driver_disconnect() {
        let executable = PathBuf::from(
            std::env::var_os("AGIT_NATIVE_CODEX_TEST_BINARY").expect("native binary"),
        );
        let root = tempfile::Builder::new()
            .prefix("agit-native-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = root.path().join("runtime");
        std::fs::create_dir(&home).unwrap();
        struct Cleanup(u32);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                // This process group is created by this isolated test, never discovered.
                unsafe {
                    libc::kill(-(self.0 as i32), libc::SIGTERM);
                }
            }
        }
        let source = crate::rc::native_codex::Source::new(&home, &executable, None).unwrap();
        let socket_directory = source.socket().parent().unwrap();
        std::fs::create_dir(socket_directory).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket_directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        drop(std::os::unix::net::UnixListener::bind(source.socket()).unwrap());
        assert!(source.socket().exists());
        let spec = LaunchSpec {
            cwd: root.path().to_path_buf(),
            resume_from: None,
            agit_session: None,
            model: None,
            dangerous: false,
            permission_mode: Some(PermissionMode::Plan),
        };
        let mut first = CodexDriver::launch_shared(spec.clone(), &source)
            .await
            .unwrap();
        let Transport::Shared(client) = &first.proc else {
            panic!("shared client required")
        };
        let _cleanup = Cleanup(client.started_pid().expect("isolated test owns its server"));
        first.confirm_opening().await.unwrap();
        let native = first.runtime_thread_id().unwrap().to_owned();
        let mut resume = spec;
        resume.resume_from = Some(native.clone());
        let mut second = CodexDriver::launch_shared(resume, &source).await.unwrap();
        second.confirm_opening().await.unwrap();
        assert_eq!(second.runtime_thread_id(), Some(native.as_str()));
        assert_eq!(second.permission_mode(), PermissionMode::Plan);
        assert!(second.transcript_path().unwrap().starts_with(source.home()));
        first.shutdown().await.unwrap();
        let result = second
            .command_request(
                "thread/read",
                json!({"threadId":native,"includeTurns":false}),
            )
            .await
            .unwrap();
        assert_eq!(result["thread"]["id"], native);
        second.shutdown().await.unwrap();
    }
}
