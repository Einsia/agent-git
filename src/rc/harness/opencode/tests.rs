use super::*;

fn engine() -> (tempfile::TempDir, OpenCodeEngine) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().to_path_buf();
    let path = cwd.join("native.sqlite");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, directory TEXT, time_created INTEGER, version TEXT, permission TEXT);
        CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
        CREATE TABLE part(id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, time_created INTEGER, data TEXT);").unwrap();
    db.execute(
        "INSERT INTO session VALUES ('ses_owned', 'project', NULL, ?1, 1, 'test', NULL)",
        [cwd.to_str().unwrap()],
    )
    .unwrap();
    let engine = OpenCodeEngine {
        proc: Proc::spawn("cat", &[], &cwd, &[]).unwrap(),
        spec: LaunchSpec {
            cwd,
            resume_from: None,
            agit_session: None,
            model: None,
            dangerous: false,
            permission_mode: None,
        },
        agent: "owned-agent".into(),
        phase: Phase::Ready,
        phase_request: 1,
        deadline: Instant::now() + HANDSHAKE_TIMEOUT,
        next_id: 10,
        session: Some("ses_owned".into()),
        turn: None,
        approvals: HashMap::new(),
        events: VecDeque::new(),
        snapshot: None,
        source: Some(crate::adapter::native_snapshot::Source {
            runtime: "opencode",
            session_id: "ses_owned".into(),
            path,
            database: true,
        }),
        seen_approvals: super::super::BoundedTurnIds::default(),
        exited: false,
    };
    (root, engine)
}

fn permission(id: Value, session: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission","params":{
        "sessionId":session,"toolCall":{"kind":"execute","title":"Run a command","rawInput":{"command":"echo test"}},
        "options":[{"kind":"allow_once","optionId":"once"},{"kind":"allow_always","optionId":"always"},{"kind":"reject_once","optionId":"reject"}]
    }})
}

async fn output(engine: &mut OpenCodeEngine) -> Value {
    let line = tokio::time::timeout(Duration::from_secs(5), engine.proc.next())
        .await
        .unwrap()
        .unwrap()
        .into_line();
    match line {
        Line::Json(value) => value,
        other => panic!("expected JSON, got {other:?}"),
    }
}

#[tokio::test]
async fn prompt_acceptance_requires_scoped_native_evidence_and_completes_from_a_snapshot() {
    let (_root, mut engine) = engine();
    assert_eq!(
        engine.start_turn("hello").await,
        TurnStartDispatch::Awaiting
    );
    let request = output(&mut engine).await;
    assert_eq!(request["method"], "session/prompt");
    assert!(engine.events.is_empty());
    assert!(matches!(
        engine.start_turn("duplicate").await,
        TurnStartDispatch::Resolved(TurnStartOutcome::ConcurrentNotAccepted { .. })
    ));
    engine.frame(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"ses_owned","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"answer"}}}})).await.unwrap();
    assert!(matches!(
        engine.events.pop_front(),
        Some(HarnessEvent::TurnStartResolved(
            TurnStartOutcome::Accepted {
                confirmation: TurnStartConfirmation::NotificationOnly,
                ..
            }
        ))
    ));
    assert!(
        matches!(engine.events.pop_front(), Some(HarnessEvent::Delta { text, .. }) if text == "answer")
    );
    engine
        .frame(json!({"jsonrpc":"2.0","id":request["id"],"result":{"stopReason":"end_turn"}}))
        .await
        .unwrap();
    assert!(
        engine
            .snapshot
            .as_ref()
            .is_some_and(|bytes| !bytes.is_empty())
    );
    assert!(matches!(
        engine.events.pop_front(),
        Some(HarnessEvent::TurnCompleted {
            outcome: TurnOutcome::Ok,
            ..
        })
    ));
    assert!(engine.turn.is_none());
    engine.proc.shutdown().await.unwrap();
}

#[tokio::test]
async fn approval_scope_is_single_use_and_native_request_identity_is_not_recycled() {
    let (_root, mut engine) = engine();
    engine.start_turn("hello").await;
    output(&mut engine).await;
    engine
        .frame(permission(json!("native-request"), "ses_owned"))
        .await
        .unwrap();
    let card = engine
        .events
        .iter()
        .find_map(|event| match event {
            HarnessEvent::Approval(card) => Some(card.clone()),
            _ => None,
        })
        .unwrap();
    assert!(!card.can_allow_for_session);
    let mut reply = ApprovalResponse {
        approval_id: card.approval_id,
        session_id: "logical-session".into(),
        decision: ApprovalDecision::Allow,
        scope: ApprovalScope::Session,
        message: None,
        by: None,
    };
    assert!(matches!(
        engine.answer_approval(&reply).await,
        ApprovalOutcome::ExplicitRefusal { retained: true, .. }
    ));
    reply.scope = ApprovalScope::Once;
    assert!(matches!(
        engine.answer_approval(&reply).await,
        ApprovalOutcome::Applied {
            effective_mode: None
        }
    ));
    let wire = output(&mut engine).await;
    assert_eq!(wire["id"], "native-request");
    assert_eq!(wire["result"]["outcome"]["optionId"], "once");
    assert!(matches!(
        engine.answer_approval(&reply).await,
        ApprovalOutcome::ExplicitRefusal {
            retained: false,
            ..
        }
    ));
    assert!(
        engine
            .frame(permission(json!("native-request"), "ses_owned"))
            .await
            .is_err()
    );
    engine.proc.shutdown().await.unwrap();
}

#[tokio::test]
async fn foreign_approvals_and_ambiguous_prompt_outcomes_cannot_accept_input() {
    let (_root, mut engine) = engine();
    engine.start_turn("hello").await;
    let request = output(&mut engine).await;
    assert!(
        engine
            .frame(permission(json!(4), "ses_foreign"))
            .await
            .is_err()
    );
    assert!(engine.events.is_empty());
    assert!(engine.frame(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32603,"message":"provider failed"}})).await.is_err());
    assert!(!engine.turn.as_ref().unwrap().accepted);
    engine.interrupt().await.unwrap();
    let cancel = output(&mut engine).await;
    assert_eq!(cancel["method"], "session/cancel");
    assert!(cancel.get("id").is_none());
    engine
        .frame(json!({"jsonrpc":"2.0","id":request["id"],"result":{"stopReason":"cancelled"}}))
        .await
        .unwrap();
    assert!(engine.events.iter().any(|event| matches!(
        event,
        HarnessEvent::TurnCompleted {
            outcome: TurnOutcome::Interrupted,
            ..
        }
    )));
    engine.proc.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_permissions_are_replied_to_before_the_next_turn() {
    let (_root, mut engine) = engine();
    engine.start_turn("cancel this turn").await;
    let prompt = output(&mut engine).await;
    for id in ["pending-a", "pending-b"] {
        engine
            .frame(permission(json!(id), "ses_owned"))
            .await
            .unwrap();
    }
    engine.interrupt().await.unwrap();
    assert_eq!(output(&mut engine).await["method"], "session/cancel");
    let mut replies = Vec::new();
    for _ in 0..2 {
        let reply = output(&mut engine).await;
        assert_eq!(reply["result"]["outcome"]["outcome"], "cancelled");
        replies.push(reply["id"].as_str().unwrap().to_string());
    }
    replies.sort();
    assert_eq!(replies, ["pending-a", "pending-b"]);
    engine
        .frame(permission(json!("late"), "ses_owned"))
        .await
        .unwrap();
    assert_eq!(
        output(&mut engine).await["result"]["outcome"]["outcome"],
        "cancelled"
    );
    assert!(engine.approvals.is_empty());
    assert!(
        !engine
            .events
            .iter()
            .any(|event| matches!(event, HarnessEvent::Approval(_)))
    );
    engine
        .frame(json!({"jsonrpc":"2.0","id":prompt["id"],"result":{"stopReason":"cancelled"}}))
        .await
        .unwrap();
    engine
        .frame(permission(json!("after-completion"), "ses_owned"))
        .await
        .unwrap();
    assert_eq!(
        output(&mut engine).await["result"]["outcome"]["outcome"],
        "cancelled"
    );
    engine.start_turn("request another tool").await;
    output(&mut engine).await;
    engine
        .frame(permission(json!("next-turn"), "ses_owned"))
        .await
        .unwrap();
    assert!(
        engine
            .events
            .iter()
            .any(|event| matches!(event, HarnessEvent::Approval(_)))
    );
    assert_eq!(engine.approvals.len(), 1);
    engine.proc.shutdown().await.unwrap();
}

#[tokio::test]
async fn exit_snapshots_include_rows_written_after_the_last_completed_turn() {
    for provider_error in [false, true] {
        let (_root, mut engine) = engine();
        engine.read_snapshot().await.unwrap();
        if provider_error {
            engine.proc.shutdown().await.unwrap();
            engine.proc = Proc::spawn("sh", &[
                "-c".into(),
                "read -r prompt; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":10,\"error\":{\"code\":-32603,\"message\":\"provider failed\"}}'; while read -r line; do :; done".into(),
            ], &engine.spec.cwd, &[]).unwrap();
        }
        let db = rusqlite::Connection::open(&engine.source.as_ref().unwrap().path).unwrap();
        db.execute_batch("INSERT INTO message VALUES ('user','ses_owned',2,'{\"role\":\"user\"}');
            INSERT INTO part VALUES ('prompt','ses_owned','user',3,'{\"type\":\"text\",\"text\":\"durable prompt\"}');
            INSERT INTO message VALUES ('assistant','ses_owned',4,'{\"role\":\"assistant\"}');
            INSERT INTO part VALUES ('answer','ses_owned','assistant',5,'{\"type\":\"text\",\"text\":\"durable partial answer\"}');").unwrap();
        assert_eq!(
            engine.start_turn("pending prompt").await,
            TurnStartDispatch::Awaiting
        );
        let (commands, receive) = mpsc::channel(1);
        let (events, mut output) = mpsc::channel(8);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let task = tokio::spawn(run_engine(engine, receive, events, shared.clone()));
        if provider_error {
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(5), output.recv())
                    .await
                    .unwrap(),
                Some(HarnessEvent::ProtocolInvariant { .. })
            ));
        } else {
            commands.send(Command::Shutdown).await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let snapshot = shared.lock().unwrap().snapshot.take().unwrap();
        assert!(snapshot.finalized);
        let bytes = snapshot.bytes.unwrap();
        let (items, _) = crate::rc::supervisor::native_records::NativeRecords::default()
            .project_final(
                &bytes,
                false,
                &crate::domain::redact::Redactor::this_machine(),
            )
            .unwrap();
        assert!(
            items
                .iter()
                .any(|item| item.event.text.as_deref() == Some("durable prompt"))
        );
        assert!(
            items
                .iter()
                .any(|item| item.event.text.as_deref() == Some("durable partial answer"))
        );
    }
}

#[tokio::test]
async fn final_snapshot_failure_does_not_invalidate_process_termination() {
    let (_root, mut engine) = engine();
    engine.read_snapshot().await.unwrap();
    std::fs::remove_file(&engine.source.as_ref().unwrap().path).unwrap();
    let (commands, receive) = mpsc::channel(1);
    commands.send(Command::Shutdown).await.unwrap();
    let (events, _output) = mpsc::channel(1);
    let shared = Arc::new(Mutex::new(Shared::default()));
    run_engine(engine, receive, events, shared.clone())
        .await
        .unwrap();
    let snapshot = shared.lock().unwrap().snapshot.take().unwrap();
    assert!(snapshot.finalized);
    assert!(snapshot.bytes.unwrap_err().contains("Resume the session"));
}

#[tokio::test]
async fn native_permission_overrides_prevent_remote_resume() {
    let (_root, mut engine) = engine();
    engine.read_snapshot().await.unwrap();
    let db = rusqlite::Connection::open(&engine.source.as_ref().unwrap().path).unwrap();
    db.execute(
        "UPDATE session SET permission = ?1",
        [r#"[{"permission":"*","pattern":"*","action":"allow"}]"#],
    )
    .unwrap();
    assert!(
        engine
            .read_snapshot()
            .await
            .unwrap_err()
            .to_string()
            .contains("permission overrides")
    );
    db.execute("UPDATE session SET permission = '[]'", [])
        .unwrap();
    engine.read_snapshot().await.unwrap();
    db.execute(
        "UPDATE session SET directory = ?1",
        [std::env::temp_dir().to_str().unwrap()],
    )
    .unwrap();
    assert!(engine.read_snapshot().await.is_err());
    engine.proc.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "Requires an isolated native OpenCode installation and synthetic provider"]
async fn native_exit_captures_durable_incomplete_turns() {
    let fail = std::env::var("AGIT_OPENCODE_SMOKE_FAILURE").as_deref() == Ok("1");
    let mut driver = OpenCodeDriver::launch(LaunchSpec {
        cwd: PathBuf::from(std::env::var("AGIT_OPENCODE_SMOKE_PROJECT").unwrap()),
        resume_from: None,
        agit_session: None,
        model: None,
        dangerous: false,
        permission_mode: None,
    })
    .await
    .unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(60), driver.next_event())
            .await
            .unwrap(),
        Some(HarnessEvent::Ready { .. })
    ));
    driver.take_snapshot();
    assert_eq!(
        driver.start_turn("Capture the native exit probe.").await,
        TurnStartDispatch::Awaiting
    );
    loop {
        match tokio::time::timeout(Duration::from_secs(30), driver.next_event())
            .await
            .unwrap()
            .unwrap()
        {
            HarnessEvent::Delta { .. } if !fail => break,
            HarnessEvent::ProtocolInvariant { .. } if fail => break,
            HarnessEvent::Approval(card) => {
                assert!(matches!(
                    driver
                        .answer_approval(&ApprovalResponse {
                            approval_id: card.approval_id,
                            session_id: "smoke".into(),
                            decision: ApprovalDecision::Allow,
                            scope: ApprovalScope::Once,
                            message: None,
                            by: None,
                        })
                        .await,
                    ApprovalOutcome::Applied { .. }
                ));
            }
            HarnessEvent::TurnStartResolved(TurnStartOutcome::Accepted { .. }) => {}
            other => panic!("unexpected native exit event: {other:?}"),
        }
    }
    driver.shutdown().await.unwrap();
    let snapshot = driver.take_snapshot().expect("final native snapshot");
    assert!(snapshot.finalized);
    let (items, _) = crate::rc::supervisor::native_records::NativeRecords::default()
        .project_final(
            &snapshot.bytes.unwrap(),
            false,
            &crate::domain::redact::Redactor::this_machine(),
        )
        .unwrap();
    assert!(
        items
            .iter()
            .any(|item| item.event.text.as_deref() == Some("Capture the native exit probe."))
    );
    assert!(
        items
            .iter()
            .any(|item| item.event.kind == crate::adapter::EventKind::ToolUse
                && item.raw["data"]["state"]["status"] == "completed")
    );
}

#[tokio::test]
#[ignore = "Requires an isolated native OpenCode installation and synthetic provider"]
async fn native_cancelled_approval_does_not_block_the_next_turn() {
    let mut driver = OpenCodeDriver::launch(LaunchSpec {
        cwd: PathBuf::from(std::env::var("AGIT_OPENCODE_SMOKE_PROJECT").unwrap()),
        resume_from: None,
        agit_session: None,
        model: None,
        dangerous: false,
        permission_mode: None,
    })
    .await
    .unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(60), driver.next_event())
            .await
            .unwrap(),
        Some(HarnessEvent::Ready { .. })
    ));
    for (prompt, cancel) in [
        ("Request the first permission probe.", true),
        ("Request the next permission probe.", false),
    ] {
        assert_eq!(driver.start_turn(prompt).await, TurnStartDispatch::Awaiting);
        let mut approvals = 0;
        loop {
            match tokio::time::timeout(Duration::from_secs(30), driver.next_event())
                .await
                .unwrap()
                .unwrap()
            {
                HarnessEvent::Approval(card) => {
                    approvals += 1;
                    if cancel {
                        driver.interrupt().await.unwrap();
                    } else {
                        assert!(matches!(
                            driver
                                .answer_approval(&ApprovalResponse {
                                    approval_id: card.approval_id,
                                    session_id: "smoke".into(),
                                    decision: ApprovalDecision::Deny,
                                    scope: ApprovalScope::Once,
                                    message: None,
                                    by: None,
                                })
                                .await,
                            ApprovalOutcome::Applied { .. }
                        ));
                    }
                }
                HarnessEvent::TurnCompleted { outcome, .. } => {
                    assert_eq!(
                        outcome,
                        if cancel {
                            TurnOutcome::Interrupted
                        } else {
                            TurnOutcome::Ok
                        }
                    );
                    assert_eq!(approvals, 1);
                    break;
                }
                HarnessEvent::TurnStartResolved(TurnStartOutcome::Accepted { .. })
                | HarnessEvent::Delta { .. } => {}
                other => panic!("unexpected native event: {other:?}"),
            }
        }
    }
    driver.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "Requires an isolated native OpenCode installation and synthetic provider"]
async fn native_opencode_smoke() {
    let cwd = PathBuf::from(
        std::env::var("AGIT_OPENCODE_SMOKE_PROJECT").expect("isolated smoke project"),
    );
    let mut resume = None;
    for recovering in [false, true] {
        let mut driver = OpenCodeDriver::launch(LaunchSpec {
            cwd: cwd.clone(),
            resume_from: resume.clone(),
            agit_session: None,
            model: None,
            dangerous: false,
            permission_mode: None,
        })
        .await
        .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(60), driver.next_event())
            .await
            .unwrap()
            .unwrap();
        let HarnessEvent::Ready {
            runtime_thread_id, ..
        } = event
        else {
            panic!("native readiness: {event:?}");
        };
        if recovering {
            assert_eq!(resume.as_ref(), Some(&runtime_thread_id));
        }
        resume = Some(runtime_thread_id);
        assert!(driver.take_snapshot().is_some());
        assert_eq!(
            driver
                .start_turn("Return the synthetic smoke response.")
                .await,
            TurnStartDispatch::Awaiting
        );
        let mut accepted = false;
        let mut interrupted = false;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(60), driver.next_event())
                .await
                .unwrap()
                .unwrap();
            match event {
                HarnessEvent::TurnStartResolved(TurnStartOutcome::Accepted { .. }) => {
                    accepted = true
                }
                HarnessEvent::Approval(card) => {
                    if std::env::var("AGIT_OPENCODE_SMOKE_CANCEL").as_deref() == Ok("1") {
                        driver.interrupt().await.unwrap();
                        interrupted = true;
                        continue;
                    }
                    let decision =
                        if std::env::var("AGIT_OPENCODE_SMOKE_ALLOW").as_deref() == Ok("1") {
                            ApprovalDecision::Allow
                        } else {
                            ApprovalDecision::Deny
                        };
                    assert!(matches!(
                        driver
                            .answer_approval(&ApprovalResponse {
                                approval_id: card.approval_id,
                                session_id: "smoke".into(),
                                decision,
                                scope: ApprovalScope::Once,
                                message: None,
                                by: None
                            })
                            .await,
                        ApprovalOutcome::Applied { .. }
                    ));
                }
                HarnessEvent::TurnCompleted { outcome, .. } => {
                    assert_eq!(
                        outcome,
                        if interrupted {
                            TurnOutcome::Interrupted
                        } else {
                            TurnOutcome::Ok
                        }
                    );
                    break;
                }
                HarnessEvent::Delta { .. } => {}
                other => panic!("unexpected native event: {other:?}"),
            }
        }
        assert!(accepted);
        let bytes = driver
            .take_snapshot()
            .expect("native completion snapshot")
            .bytes
            .unwrap();
        use crate::adapter::Adapter;
        let transcript = crate::adapter::opencode::OpenCode
            .parse(std::str::from_utf8(&bytes).unwrap())
            .unwrap();
        assert!(
            transcript
                .events
                .iter()
                .any(|event| event.kind == crate::adapter::EventKind::UserPrompt)
        );
        driver.shutdown().await.unwrap();
    }
}
