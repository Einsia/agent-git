//! Fallback release and foreground admission belong to one supervisor generation.

use super::*;
use std::time::Duration;

fn instruction(command: &Command) -> bool {
    match command {
        Command::Model { model, .. } => model.is_some(),
        Command::Runtime {
            name, arguments, ..
        } => match name.as_str() {
            "commands" | "status" | "diff" | "mcp" | "skills" | "apps" | "prompt.expand"
            | "skill.expand" => false,
            "goal" | "fast" | "personality" | "memories" => arguments["value"]
                .as_str()
                .is_some_and(|value| !value.trim().is_empty()),
            _ => true,
        },
        Command::Shutdown | Command::ClaudeRestartGuardReady => false,
        _ => true,
    }
}

impl Session {
    pub(super) async fn prepare_fallback_instruction(
        &mut self,
        command: &Command,
    ) -> crate::Result<()> {
        if !instruction(command) {
            return Ok(());
        }
        if let Command::Turn {
            message,
            attribution,
            ..
        } = command
            && (self
                .pending_turn_command
                .as_ref()
                .is_some_and(|pending| pending.initial)
                || self.resolved_initial_turn.as_ref().is_some_and(|resolved| {
                    resolved.message == *message && resolved.attribution.same_sender(attribution)
                }))
        {
            // Receipt lookup and creation-prompt coalescing do not represent new human intent.
            return Ok(());
        }
        self.check_control_instruction(command)?;
        let AnyDriver::Codex(driver) = &mut self.driver else {
            return Ok(());
        };
        let backgrounded = driver.background_control();
        let result = driver.resume_background().await;
        self.drain_control_events().await;
        result?;
        self.check_control_instruction(command)?;
        anyhow::ensure!(
            self.info.status != SessionStatus::Ended,
            "native control is no longer available; no instruction was sent"
        );
        if backgrounded
            && !matches!(
                command,
                Command::SetPermissionMode { .. }
                    | Command::Interrupt { .. }
                    | Command::Approve { .. }
            )
        {
            // Admission must not use the policy that was cached before another native owner ran.
            anyhow::ensure!(
                self.driver.permission_mode_known(),
                "native permissions are unknown after reconnecting; no instruction was sent"
            );
        }
        Ok(())
    }

    fn check_control_instruction(&self, command: &Command) -> crate::Result<()> {
        let requires_owner = self.info.dangerous || !self.driver.permission_mode_known();
        match command {
            Command::Turn { reply, .. } => reply.check_control(requires_owner),
            Command::Steer { reply, .. } => reply.check_control(requires_owner),
            Command::Model { reply, .. }
            | Command::Runtime { reply, .. }
            | Command::Enqueue { reply, .. } => reply.check_control(requires_owner),
            Command::Interrupt { reply, .. } => reply.check_control(false),
            Command::Approve { reply, .. } => reply.check_control(false),
            Command::SetPermissionMode { reply, mode, .. } => {
                reply.check_control(mode.loosens_from(self.driver.permission_mode()))
            }
            Command::InitialTurn { .. } | Command::Shutdown | Command::ClaudeRestartGuardReady => {
                Ok(())
            }
        }
    }

    async fn drain_control_events(&mut self) {
        loop {
            let event = match &mut self.driver {
                AnyDriver::Codex(driver) => driver.take_control_event(),
                _ => None,
            };
            let Some(event) = event else {
                break;
            };
            self.on_harness_event_with_commands(event, None).await;
            if self.info.status == SessionStatus::Ended {
                break;
            }
        }
    }

    pub(super) async fn poll_fallback_control(&mut self) {
        let observation_ready = self.transcript_readable;
        let event = match &mut self.driver {
            AnyDriver::Codex(driver) => driver.background_after_silence(observation_ready).await,
            _ => None,
        };
        if let Some(event) = event {
            self.on_harness_event_with_commands(event, None).await;
            self.emit(
                method::SESSION_MODEL,
                serde_json::json!({"session_id":self.info.session_id}),
            )
            .await;
        }
        let AnyDriver::Codex(driver) = &mut self.driver else {
            return;
        };
        if !driver.background_control() || tokio::time::Instant::now() < self.background_poll_at {
            return;
        }
        self.background_poll_at = tokio::time::Instant::now() + Duration::from_secs(5);
        let observation = driver.background_observation().await;
        self.drain_control_events().await;
        if let Ok(status) = observation {
            let waiting = status["activeFlags"].as_array().is_some_and(|flags| {
                flags
                    .iter()
                    .any(|flag| flag == "waitingOnApproval" || flag == "waitingOnUserInput")
            });
            if waiting {
                self.set_status(SessionStatus::AwaitingApproval).await;
            } else if self.pending.is_empty() && self.driver.has_active_turn() {
                self.set_status(SessionStatus::Running).await;
            }
        }
    }

    pub(super) async fn reject_fallback_instruction(
        &mut self,
        command: Command,
        error: anyhow::Error,
    ) {
        if let Some(arm) = command_danger_arm(&command) {
            let _ = self
                .notes
                .send(SessionNote::DangerDisarmed {
                    session_id: self.info.session_id.clone(),
                    generation: self.generation,
                    arm,
                })
                .await;
        }
        match command {
            Command::Turn { reply, .. } => {
                reply.finish(Ok(TurnStartOutcome::RetryableNotAccepted {
                    message: format!(
                        "Could not reconnect native control; no message was sent: {error}"
                    ),
                }))
            }
            Command::Steer { reply, .. } => reply.finish(Err(error)),
            Command::Interrupt { reply, .. } => reply.finish(Err(error)),
            Command::Approve { reply, .. } => reply.finish(Err(error)),
            Command::SetPermissionMode { reply, .. } => reply.finish(Err(error)),
            Command::Model { reply, .. }
            | Command::Runtime { reply, .. }
            | Command::Enqueue { reply, .. } => reply.finish(Err(error)),
            Command::InitialTurn { .. } => {
                self.on_harness_event_with_commands(
                    HarnessEvent::Progress {
                        text: format!("The initial instruction was not sent: {error}"),
                    },
                    None,
                )
                .await;
            }
            Command::Shutdown | Command::ClaudeRestartGuardReady => {}
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    #[ignore = "requires an isolated Codex home and paused localhost approval provider"]
    async fn real_background_approval_returns_through_supervisor_without_another_turn() {
        let home = std::path::PathBuf::from(std::env::var("AGIT_TEST_BACKGROUND_HOME").unwrap());
        let executable =
            std::path::PathBuf::from(std::env::var("AGIT_TEST_BACKGROUND_EXECUTABLE").unwrap());
        let source = crate::rc::native_codex::Source::new(&home, &executable, None).unwrap();
        let mut driver = crate::rc::harness::codex::CodexDriver::launch_shared(
            LaunchSpec {
                cwd: home.join("project"),
                resume_from: None,
                agit_session: None,
                model: None,
                dangerous: false,
                permission_mode: Some(PermissionMode::Default),
            },
            &source,
        )
        .await
        .unwrap();
        driver.confirm_opening().await.unwrap();
        assert!(matches!(
            driver
                .start_turn("Request the isolated fixture approval.", true, None)
                .await,
            TurnStartDispatch::Awaiting
        ));
        loop {
            let event = tokio::time::timeout(Duration::from_secs(15), driver.next_event())
                .await
                .unwrap()
                .unwrap();
            if let HarnessEvent::TurnStartResolved(outcome) = event {
                assert!(matches!(outcome, TurnStartOutcome::Accepted { .. }));
                break;
            }
        }
        driver.expire_test_fallback_control();
        let transcript = driver.transcript_path().unwrap();
        let (mut session, mut output, mut notes) =
            super::super::tests::harness_test_session_with_channels(
                AnyDriver::Codex(Box::new(driver)),
                "codex",
                SessionStatus::Running,
            );
        session.tailer = Some(Tailer::new(transcript.clone(), true));
        session.transcript_readable = true;
        let persistence = tokio::spawn(async move {
            while let Some(note) = notes.recv().await {
                if let SessionNote::NativeSettings { ack, .. } = note {
                    let _ = ack.send(Ok(()));
                }
            }
        });
        let (events, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let forwarding = tokio::spawn(async move {
            while let Some(frame) = output.recv().await {
                let _ = events.send(frame);
            }
        });
        session.poll_fallback_control().await;
        let AnyDriver::Codex(driver) = &session.driver else {
            unreachable!()
        };
        assert!(driver.background_control());
        std::fs::write(home.join("continue-fixture"), "continue").unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            while session.info.status != SessionStatus::AwaitingApproval {
                session.background_poll_at = tokio::time::Instant::now();
                session.poll_fallback_control().await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            session.pending.is_empty(),
            "background observation must not invent approval details"
        );
        let foreground = Command::Runtime {
            name: "foreground".into(),
            arguments: json!({}),
            reply: crate::rc::ticket::ticket().0,
        };
        session
            .prepare_fallback_instruction(&foreground)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            while session.pending.is_empty() {
                let event = session.driver.next_event().await.unwrap();
                session.on_harness_event(event).await;
            }
        })
        .await
        .unwrap();
        let approval_id = session.pending.keys().next().unwrap().clone();
        let (ticket, mut receipt) = crate::rc::ticket::ticket();
        let (commands, mut inbox) = mpsc::channel(1);
        commands
            .send(Command::Approve {
                response: ApprovalResponse {
                    approval_id,
                    session_id: session.info.session_id.clone(),
                    decision: crate::protocol::ApprovalDecision::Deny,
                    scope: crate::protocol::ApprovalScope::Once,
                    message: None,
                    answers: None,
                    by: Some("fixture-owner".into()),
                },
                caller_is_owner: true,
                danger: DangerAuthorization::NotRequired,
                reply: ticket,
            })
            .await
            .unwrap();
        let task = tokio::spawn(async move {
            session.run_inner(&mut inbox).await;
            session
        });
        let outcome = receipt
            .wait(Duration::from_secs(15))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(outcome, ApprovalOutcome::Applied { .. }));
        tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(frame) = observed.recv().await {
                if frame.method() == method::TURN_COMPLETED {
                    return;
                }
            }
            panic!("background task completion was not published");
        })
        .await
        .unwrap();
        drop(commands);
        let mut session = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
        assert!(session.pending.is_empty());
        assert!(
            std::fs::read_to_string(transcript)
                .unwrap()
                .contains("isolated fixture answer 2")
        );
        session.driver.shutdown().await.unwrap();
        persistence.abort();
        forwarding.abort();
    }

    #[tokio::test]
    async fn observation_does_not_renew_fallback_control_or_stop_the_active_session() {
        let mut driver = crate::rc::harness::codex::CodexDriver::test_responder(
            Some("thread"),
            &[
                json!({"id":1,"result":{"config":{"thread_unload_delay_secs":1}}}),
                json!({"id":2,"result":{"status":"unsubscribed"}}),
                json!({"id":3,"result":{"thread":{"id":"thread","cwd":"/","status":{"type":"active","activeFlags":["waitingOnApproval"]}}}}),
                json!({"id":4,"result":{"data":[{"id":"active","status":"inProgress"}]}}),
            ],
        );
        driver.set_test_current_turn("active");
        driver.expire_test_fallback_control();
        let (mut session, mut output, mut notes) =
            super::super::tests::harness_test_session_with_channels(
                AnyDriver::Codex(Box::new(driver)),
                "codex",
                SessionStatus::Running,
            );
        let file = tempfile::NamedTempFile::new().unwrap();
        session.tailer = Some(Tailer::new(file.path().to_path_buf(), true));
        let read = Command::Model {
            model: None,
            reply: crate::rc::ticket::ticket().0,
        };
        session.prepare_fallback_instruction(&read).await.unwrap();
        session.drain_transcript().await;
        let poll = async {
            session.poll_fallback_control().await;
        };
        let persist = async {
            let Some(SessionNote::NativeSettings { mode, ack, .. }) = notes.recv().await else {
                panic!("background policy must be persisted");
            };
            assert!(mode.is_none());
            ack.send(Ok(())).unwrap();
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(poll, persist);
        })
        .await
        .unwrap();
        assert_eq!(session.info.status, SessionStatus::AwaitingApproval);
        let AnyDriver::Codex(driver) = &session.driver else {
            panic!("expected Codex");
        };
        assert!(driver.background_control());
        assert!(session.driver.has_active_turn());
        assert!(
            output
                .try_recv()
                .is_ok_and(|frame| frame.method() == "session.progress")
        );
        session.driver.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn human_intent_and_current_policy_are_distinct_from_observation() {
        for name in ["commands", "status", "diff", "mcp", "skills", "apps"] {
            let read = Command::Runtime {
                name: name.into(),
                arguments: json!({}),
                reply: crate::rc::ticket::ticket().0,
            };
            assert!(!instruction(&read));
        }
        let write = Command::Runtime {
            name: "goal".into(),
            arguments: json!({"value":"Continue work"}),
            reply: crate::rc::ticket::ticket().0,
        };
        assert!(instruction(&write));
        let driver = crate::rc::harness::codex::CodexDriver::test_responder(Some("thread"), &[]);
        let (mut session, _output, _notes) =
            super::super::tests::harness_test_session_with_channels(
                AnyDriver::Codex(Box::new(driver)),
                "codex",
                SessionStatus::Idle,
            );
        let turn = Command::Turn {
            message: "instruction".into(),
            attribution: Default::default(),
            guard_attempt: None,
            reply: crate::rc::ticket::ticket().0.with_control_ceiling(false),
        };
        assert!(instruction(&turn));
        session.check_control_instruction(&turn).unwrap();
        session.info.dangerous = true;
        assert!(session.check_control_instruction(&turn).is_err());
        let brake = Command::Interrupt {
            expected_turn_id: None,
            reply: crate::rc::ticket::ticket().0.with_control_ceiling(false),
        };
        session.check_control_instruction(&brake).unwrap();
        session.driver.shutdown().await.unwrap();
    }
}
