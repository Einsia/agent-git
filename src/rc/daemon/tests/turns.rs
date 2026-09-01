use super::*;

/// One session can spend the full receipt window waiting for its
/// supervisor without pinning the daemon state lock or another session's
/// command path. This exercises the same prepare/execute split used by the
/// LinkEvent production arm, including the per-session single-flight gate.
#[tokio::test]
async fn one_sessions_unanswered_rpc_does_not_block_another_session() {
    let (a_tx, mut a_rx) = mpsc::channel(1);
    let (b_tx, mut b_rx) = mpsc::channel(1);
    let daemon = rpc_test_daemon(
        [
            (
                "session-a".into(),
                rpc_test_live(
                    "session-a",
                    1,
                    a_tx,
                    crate::protocol::PermissionMode::Default,
                ),
            ),
            (
                "session-b".into(),
                rpc_test_live(
                    "session-b",
                    2,
                    b_tx,
                    crate::protocol::PermissionMode::Default,
                ),
            ),
        ]
        .into_iter()
        .collect(),
        Roster::default(),
    );
    let interrupt = |id: &str| {
        let mut frame = Frame::request(
            method::TURN_INTERRUPT,
            TurnInterrupt {
                session_id: id.into(),
                by: Some("operator".into()),
            },
        );
        frame.caller = Some(claim("operator", "ws-a"));
        frame
    };

    let prepared_a = daemon
        .lock()
        .await
        .prepare_session_rpc(&interrupt("session-a"))
        .expect("first session is admitted");
    let daemon_a = daemon.clone();
    let task_a = tokio::spawn(async move {
        let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
        prepared_a.execute(daemon_a, &mut stop).await
    });
    let held_a = tokio::time::timeout(std::time::Duration::from_millis(200), a_rx.recv())
        .await
        .expect("A command is queued")
        .expect("A queue stays open");

    assert!(
        daemon.try_lock().is_ok(),
        "A's unanswered receipt must not retain the daemon mutex"
    );
    let same_session_error = match daemon
        .lock()
        .await
        .prepare_session_rpc(&interrupt("session-a"))
    {
        Ok(_) => panic!("a second command entered the same live session"),
        Err(error) => error,
    };
    assert_eq!(same_session_error.code, ErrorCode::SessionBusy as i32);

    let prepared_b = tokio::time::timeout(std::time::Duration::from_millis(200), async {
        daemon
            .lock()
            .await
            .prepare_session_rpc(&interrupt("session-b"))
    })
    .await
    .expect("B is not blocked behind A")
    .expect("B is admitted");
    let daemon_b = daemon.clone();
    let task_b = tokio::spawn(async move {
        let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
        prepared_b.execute(daemon_b, &mut stop).await
    });
    let command_b = tokio::time::timeout(std::time::Duration::from_millis(200), b_rx.recv())
        .await
        .expect("B command is queued")
        .expect("B queue stays open");
    match command_b {
        Command::Interrupt { reply } => {
            assert!(reply.accept());
            reply.finish(Ok(()));
        }
        _ => panic!("wrong command for B"),
    }
    let reply_b = tokio::time::timeout(std::time::Duration::from_millis(200), task_b)
        .await
        .expect("B replies while A remains unanswered")
        .expect("B task joins");
    assert!(reply_b.pending.is_none());
    let reply_b = reply_b.response.expect("B command succeeds");
    assert_eq!(reply_b, serde_json::json!({}));

    // Ended may arrive before A's receipt wakes. It must leave a fenced
    // tombstone rather than deleting the state that completion still owns.
    {
        let mut state = daemon.lock().await;
        state.on_session_note(SessionNote::Ended {
            session_id: "session-a".into(),
            generation: 1,
        });
        assert!(
            state
                .sessions
                .get("session-a")
                .is_some_and(|live| live.ended)
        );
    }

    // Let A's unaccepted ticket close. Its waiter should finish, project
    // against generation 1, and only then remove the Ended tombstone.
    drop(held_a);
    let _ = tokio::time::timeout(std::time::Duration::from_millis(200), task_a)
        .await
        .expect("A notices its dropped supervisor ticket");
    assert!(!daemon.lock().await.sessions.contains_key("session-a"));
}

/// The lock-free execution path must serialize the protocol's declared
/// result DTOs, rather than maintaining a second handwritten wire shape.
/// This catches drift in the two queued RPC results that moved out of
/// `dispatch` when session waits stopped holding the daemon mutex.
#[tokio::test]
async fn prepared_session_rpc_serializes_declared_turn_results() {
    let (tx, mut rx) = mpsc::channel(2);
    let daemon = rpc_test_daemon(
        [(
            "session-a".into(),
            rpc_test_live("session-a", 1, tx, crate::protocol::PermissionMode::Default),
        )]
        .into_iter()
        .collect(),
        Roster::default(),
    );

    let mut turn = Frame::request(
        method::TURN_START,
        TurnStart {
            session_id: "session-a".into(),
            message: "hello".into(),
            by: Some("operator".into()),
            client_msg_id: Some("message-1".into()),
        },
    );
    turn.caller = Some(claim("operator", "ws-a"));
    let prepared = daemon
        .lock()
        .await
        .prepare_session_rpc(&turn)
        .expect("turn is admitted");
    let worker_daemon = daemon.clone();
    let worker = tokio::spawn(async move {
        let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
        prepared.execute(worker_daemon, &mut stop).await
    });
    match rx.recv().await.expect("turn command is queued") {
        Command::Turn { reply, .. } => {
            assert!(reply.accept());
            reply.finish(Ok(TurnStartOutcome::Accepted {
                turn_id: "turn-42".into(),
                still_running: true,
                consumed_mode: None,
                confirmation: TurnStartConfirmation::Exact,
            }));
        }
        _ => panic!("wrong queued command"),
    }
    let turn_result = worker
        .await
        .expect("turn worker joins")
        .response
        .expect("turn succeeds");
    assert_eq!(
        turn_result,
        serde_json::to_value(TurnStartResult {
            turn_id: "turn-42".into(),
        })
        .unwrap()
    );
    let _: TurnStartResult =
        serde_json::from_value(turn_result).expect("declared turn result decodes");

    let mut interrupt = Frame::request(
        method::TURN_INTERRUPT,
        TurnInterrupt {
            session_id: "session-a".into(),
            by: Some("operator".into()),
        },
    );
    interrupt.caller = Some(claim("operator", "ws-a"));
    let prepared = daemon
        .lock()
        .await
        .prepare_session_rpc(&interrupt)
        .expect("interrupt is admitted after the turn gate is released");
    let worker_daemon = daemon.clone();
    let worker = tokio::spawn(async move {
        let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
        prepared.execute(worker_daemon, &mut stop).await
    });
    match rx.recv().await.expect("interrupt command is queued") {
        Command::Interrupt { reply } => {
            assert!(reply.accept());
            reply.finish(Ok(()));
        }
        _ => panic!("wrong queued command"),
    }
    let interrupt_result = worker
        .await
        .expect("interrupt worker joins")
        .response
        .expect("interrupt succeeds");
    assert_eq!(
        interrupt_result,
        serde_json::to_value(TurnInterruptResult::default()).unwrap()
    );
    let _: TurnInterruptResult =
        serde_json::from_value(interrupt_result).expect("declared interrupt result decodes");
}

#[test]
fn a_taken_turn_with_a_lost_typed_outcome_never_releases_its_gate() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let (tx, mut rx) = mpsc::channel(1);
                let mut live = rpc_test_live("session-a", 1, tx, PermissionMode::Auto);
                live.pending_mode = Some(PermissionMode::Plan);
                let gate = live.rpc_gate.clone();
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "session-a".into(),
                    rpc_test_roster_entry("session-a", PermissionMode::Auto),
                );
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
                let mut frame = Frame::request(
                    method::TURN_START,
                    TurnStart {
                        session_id: "session-a".into(),
                        message: "possibly accepted".into(),
                        by: Some("owner".into()),
                        client_msg_id: Some("message-1".into()),
                    },
                );
                frame.caller = Some(claim("owner", "ws-a"));
                let prepared = daemon.lock().await.prepare_session_rpc(&frame).unwrap();
                let worker_daemon = daemon.clone();
                let mut worker = tokio::spawn(async move {
                    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
                    prepared.execute(worker_daemon, &mut stop).await
                });
                match rx.recv().await.unwrap() {
                    Command::Turn { reply, .. } => {
                        assert!(reply.accept());
                        drop(reply);
                    }
                    _ => panic!("wrong command"),
                }

                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(20), &mut worker)
                        .await
                        .is_err(),
                    "a closed taken ticket is not termination proof"
                );
                assert!(
                    gate.try_lock_owned().is_err(),
                    "the per-session gate stays held for hard-stop Plan persistence"
                );
                assert!(daemon.lock().await.sessions["session-a"].rpc_guard_sensitive);
                worker.abort();
                let _ = worker.await;
            });
    });
}

#[test]
fn accepted_turn_mode_waits_for_durable_projection_before_reply() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let (tx, mut rx) = mpsc::channel(1);
                let mut live = rpc_test_live("session-a", 1, tx, PermissionMode::Auto);
                live.pending_mode = Some(PermissionMode::Plan);
                let gate = live.rpc_gate.clone();
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "session-a".into(),
                    rpc_test_roster_entry("session-a", PermissionMode::Auto),
                );
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
                let mut frame = Frame::request(
                    method::TURN_START,
                    TurnStart {
                        session_id: "session-a".into(),
                        message: "inspect".into(),
                        by: Some("owner".into()),
                        client_msg_id: Some("message-1".into()),
                    },
                );
                frame.caller = Some(claim("owner", "ws-a"));
                let prepared = daemon
                    .lock()
                    .await
                    .prepare_session_rpc(&frame)
                    .expect("turn is admitted");
                let worker_daemon = daemon.clone();
                let worker = tokio::spawn(async move {
                    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
                    prepared.execute(worker_daemon, &mut stop).await
                });
                let reply = match rx.recv().await.expect("turn reaches supervisor") {
                    Command::Turn { reply, .. } => {
                        assert!(reply.accept());
                        reply
                    }
                    _ => panic!("wrong command"),
                };

                roster::fail_next_saves(1, 1);
                reply.finish(Ok(TurnStartOutcome::Accepted {
                    turn_id: "turn-1".into(),
                    still_running: true,
                    consumed_mode: Some(PermissionMode::Plan),
                    confirmation: TurnStartConfirmation::Exact,
                }));
                while roster::pending_injected_saves() != (0, 0) {
                    tokio::task::yield_now().await;
                }

                assert!(!worker.is_finished(), "no ACK before a durable save");
                assert!(
                    daemon.try_lock().is_ok(),
                    "retry backoff releases daemon mutex"
                );
                assert!(
                    gate.clone().try_lock_owned().is_err(),
                    "the per-session gate remains held across persistence retry"
                );
                {
                    let state = daemon.lock().await;
                    assert!(state.sessions["session-a"].rpc_guard_sensitive);
                }

                tokio::time::advance(FAIL_CLOSED_PERSIST_RETRY_MIN).await;
                let executed = worker.await.unwrap();
                assert!(executed.response.is_ok());
                assert!(gate.try_lock_owned().is_ok());
                let state = daemon.lock().await;
                assert_eq!(
                    state.sessions["session-a"].info.permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert_eq!(state.sessions["session-a"].pending_mode, None);
                assert!(!state.sessions["session-a"].rpc_guard_sensitive);
                drop(state);
                assert_eq!(
                    Roster::load().sessions["session-a"].permission_mode,
                    Some(PermissionMode::Plan)
                );
            });
    });
}

#[test]
fn successful_mode_change_waits_for_its_exact_durable_projection_before_ack() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::{PermissionApply, PermissionMode};

                let (tx, mut rx) = mpsc::channel(1);
                let live = rpc_test_live("session-a", 1, tx, PermissionMode::Auto);
                let gate = live.rpc_gate.clone();
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "session-a".into(),
                    rpc_test_roster_entry("session-a", PermissionMode::Auto),
                );
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
                let prepared = daemon
                    .lock()
                    .await
                    .prepare_session_rpc(&rpc_mode_frame(PermissionMode::Plan))
                    .expect("mode change is admitted");
                let worker_daemon = daemon.clone();
                let worker = tokio::spawn(async move {
                    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
                    prepared.execute(worker_daemon, &mut stop).await
                });
                let reply = match rx.recv().await.expect("mode command reaches supervisor") {
                    Command::SetPermissionMode { reply, .. } => {
                        assert!(reply.accept());
                        reply
                    }
                    _ => panic!("wrong command"),
                };

                roster::fail_next_saves(1, 1);
                reply.finish(Ok(PermissionModeOutcome::Applied {
                    applied: PermissionApply::Immediate,
                }));
                while roster::pending_injected_saves() != (0, 0) {
                    tokio::task::yield_now().await;
                }

                assert!(!worker.is_finished(), "no ACK before a durable save");
                assert!(
                    daemon.try_lock().is_ok(),
                    "retry does not hold daemon mutex"
                );
                assert!(
                    gate.clone().try_lock_owned().is_err(),
                    "the per-session gate stays held through persistence retry"
                );
                {
                    let state = daemon.lock().await;
                    assert!(state.sessions["session-a"].rpc_guard_sensitive);
                }

                tokio::time::advance(FAIL_CLOSED_PERSIST_RETRY_MIN).await;
                let executed = worker.await.unwrap();
                assert!(executed.response.is_ok());
                assert!(gate.try_lock_owned().is_ok());
                let state = daemon.lock().await;
                assert_eq!(
                    state.sessions["session-a"].info.permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert!(!state.sessions["session-a"].rpc_guard_sensitive);
                drop(state);
                assert_eq!(
                    Roster::load().sessions["session-a"].permission_mode,
                    Some(PermissionMode::Plan)
                );
            });
    });
}

#[test]
fn an_unknown_mode_change_persists_a_stable_plan_floor_before_reply() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::{PermissionApply, PermissionMode};

                let (tx, mut rx) = mpsc::channel(1);
                let mut live = rpc_test_live("session-a", 1, tx, PermissionMode::Bypass);
                live.info.dangerous = true;
                let gate = live.rpc_gate.clone();
                let mut roster = Roster::default();
                let mut entry = rpc_test_roster_entry("session-a", PermissionMode::Bypass);
                entry.ever_dangerous = true;
                entry.runtime = "unsupported-test-runtime".into();
                entry.cwd = home.path().to_string_lossy().into_owned();
                roster.sessions.insert("session-a".into(), entry);
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
                daemon
                    .lock()
                    .await
                    .mirror
                    .bind("ws-a", "project-a", home.path())
                    .unwrap();
                let prepared = daemon
                    .lock()
                    .await
                    .prepare_session_rpc(&rpc_mode_frame(PermissionMode::Plan))
                    .expect("tightening is admitted");
                let recovery_token = match &prepared.operation {
                    SessionRpcOperation::SetPermissionMode { recovery_token, .. } => {
                        recovery_token.clone()
                    }
                    _ => unreachable!(),
                };
                assert!(recovery_token.starts_with(roster::SHUTDOWN_GUARD_PREFIX));
                let worker_daemon = daemon.clone();
                let worker = tokio::spawn(async move {
                    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
                    prepared.execute(worker_daemon, &mut stop).await
                });
                let reply = match rx.recv().await.expect("mode command reaches supervisor") {
                    Command::SetPermissionMode { reply, .. } => {
                        assert!(reply.accept());
                        reply
                    }
                    _ => panic!("wrong command"),
                };

                roster::fail_next_saves(1, 1);
                reply.finish(Ok(PermissionModeOutcome::Unknown {
                    message: "native receipt was ambiguous".into(),
                }));
                while roster::pending_injected_saves() != (0, 0) {
                    tokio::task::yield_now().await;
                }

                assert!(!worker.is_finished(), "no error reply before durable Plan");
                assert!(gate.clone().try_lock_owned().is_err());
                {
                    let mut state = daemon.lock().await;
                    assert!(
                        state.sessions["session-a"].ended,
                        "the typed terminal completion fences Live before the later Ended note"
                    );
                    assert_eq!(
                        state.sessions["session-a"].info.status,
                        SessionStatus::Ended
                    );
                    assert_eq!(
                        state.sessions["session-a"].info.permission_mode,
                        Some(PermissionMode::Plan)
                    );
                    assert_eq!(state.sessions["session-a"].pending_mode, None);
                    let attempt =
                        &state.roster.sessions["session-a"].guard_attempts[&recovery_token];
                    assert_eq!(attempt.expected_mode, PermissionMode::Plan);
                    assert!(!attempt.observed);
                    assert_eq!(
                        state.roster.sessions["session-a"].permission_mode,
                        Some(PermissionMode::Plan)
                    );

                    state
                        .latest_session_generations
                        .insert("session-a".into(), 1);
                    let loose = tagged_test_notification(
                        "session-a",
                        1,
                        method::SESSION_PERMISSION_MODE,
                        serde_json::to_value(crate::protocol::SessionPermissionMode {
                            session_id: "session-a".into(),
                            mode: PermissionMode::Bypass,
                            applied: PermissionApply::Immediate,
                            by: Some("old-command".into()),
                        })
                        .unwrap(),
                    );
                    assert!(
                        state.project_session_frame(loose).is_none(),
                        "the recovery S fences a loose frame queued before Unknown"
                    );
                    assert_eq!(state.journal.last_seq("session-a"), 0);
                    assert_eq!(
                        state.sessions["session-a"].info.permission_mode,
                        Some(PermissionMode::Plan)
                    );

                    let (frames, _frames_rx) = mpsc::channel(1);
                    let error = state
                        .resume_session(
                            SessionResume {
                                workspace_id: "ws-a".into(),
                                session_id: "session-a".into(),
                                prompt: None,
                                by: Some("owner".into()),
                                agent: None,
                                expected_agent_id: None,
                                branch: None,
                            },
                            &claim("owner", "ws-a"),
                            &frames,
                        )
                        .await
                        .expect_err("resume cannot return a dead Live during persistence retry");
                    assert_eq!(error.code, ErrorCode::SessionBusy as i32);
                }
                assert_eq!(
                    Roster::load().sessions["session-a"].permission_mode,
                    Some(PermissionMode::Bypass),
                    "both injected destinations really failed before the retry"
                );

                tokio::time::advance(FAIL_CLOSED_PERSIST_RETRY_MIN).await;
                let executed = worker.await.unwrap();
                assert!(executed.response.is_err());
                assert!(gate.try_lock_owned().is_ok());
                {
                    let mut state = daemon.lock().await;
                    assert!(
                        !state.sessions.contains_key("session-a"),
                        "durable completion removes its own Ended tombstone before replying"
                    );
                    assert!(
                        state.roster.sessions["session-a"]
                            .guard_attempts
                            .contains_key(&recovery_token),
                        "Unknown S remains until a later Plan generation reaches Ready"
                    );
                    state.on_session_note(SessionNote::Ended {
                        session_id: "session-a".into(),
                        generation: 1,
                    });
                    assert!(
                        !state.sessions.contains_key("session-a"),
                        "the later Ended note is generation-fenced and harmless"
                    );

                    let (frames, _frames_rx) = mpsc::channel(1);
                    let error = state
                        .resume_session(
                            SessionResume {
                                workspace_id: "ws-a".into(),
                                session_id: "session-a".into(),
                                prompt: None,
                                by: Some("owner".into()),
                                agent: None,
                                expected_agent_id: None,
                                branch: None,
                            },
                            &claim("owner", "ws-a"),
                            &frames,
                        )
                        .await
                        .expect_err("resume must take the roster path, not return the dead Live");
                    assert_eq!(error.code, ErrorCode::RuntimeUnavailable as i32);
                    assert!(error.message.contains("unsupported-test-runtime"));
                }
                let disk = Roster::load();
                assert_eq!(
                    disk.sessions["session-a"].permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert!(
                    disk.sessions["session-a"]
                        .guard_attempts
                        .contains_key(&recovery_token)
                );
            });
    });
}

#[test]
fn an_explicit_mode_refusal_rolls_back_only_its_arm_without_plan() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let (tx, mut rx) = mpsc::channel(1);
                let live = rpc_test_live("session-a", 1, tx, PermissionMode::Default);
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "session-a".into(),
                    rpc_test_roster_entry("session-a", PermissionMode::Default),
                );
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
                let prepared = daemon
                    .lock()
                    .await
                    .prepare_session_rpc(&rpc_mode_frame(PermissionMode::Bypass))
                    .expect("owner can prepare the loosening");
                let worker_daemon = daemon.clone();
                let worker = tokio::spawn(async move {
                    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
                    prepared.execute(worker_daemon, &mut stop).await
                });
                let reply = match rx.recv().await.unwrap() {
                    Command::SetPermissionMode { reply, .. } => {
                        assert!(reply.accept());
                        reply
                    }
                    _ => panic!("wrong command"),
                };
                reply.finish(Ok(PermissionModeOutcome::ExplicitRefusal {
                    message: "native refused".into(),
                }));

                let executed = worker.await.unwrap();
                assert!(executed.response.is_err());
                let state = daemon.lock().await;
                assert_eq!(
                    state.sessions["session-a"].info.permission_mode,
                    Some(PermissionMode::Default)
                );
                assert!(!state.sessions["session-a"].info.dangerous);
                let entry = &state.roster.sessions["session-a"];
                assert_eq!(entry.permission_mode, Some(PermissionMode::Default));
                assert!(!entry.ever_dangerous);
                assert!(!roster::has_shutdown_guard(&entry.guard_attempts));
            });
    });
}

#[test]
fn unknown_mode_keeps_its_floor_through_active_fallback_failures() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let (tx, mut rx) = mpsc::channel(1);
                let live = rpc_test_live("session-a", 1, tx, PermissionMode::Auto);
                let gate = live.rpc_gate.clone();
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "session-a".into(),
                    rpc_test_roster_entry("session-a", PermissionMode::Auto),
                );
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
                let prepared = daemon
                    .lock()
                    .await
                    .prepare_session_rpc(&rpc_mode_frame(PermissionMode::Plan))
                    .unwrap();
                let recovery_token = match &prepared.operation {
                    SessionRpcOperation::SetPermissionMode { recovery_token, .. } => {
                        recovery_token.clone()
                    }
                    _ => unreachable!(),
                };

                // Make the fallback authoritative only after prepare's
                // durable-row barrier, so the Unknown projection exercises
                // the active-fallback save path itself.
                {
                    let state = daemon.lock().await;
                    roster::fail_next_saves(1, 0);
                    state
                        .roster
                        .save_fail_closed()
                        .expect("establish authoritative fallback");
                }
                let worker_daemon = daemon.clone();
                let worker = tokio::spawn(async move {
                    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
                    prepared.execute(worker_daemon, &mut stop).await
                });
                let reply = match rx.recv().await.unwrap() {
                    Command::SetPermissionMode { reply, .. } => {
                        assert!(reply.accept());
                        reply
                    }
                    _ => panic!("wrong command"),
                };

                roster::fail_next_saves(0, 1);
                reply.finish(Ok(PermissionModeOutcome::Unknown {
                    message: "unknown on active fallback".into(),
                }));
                while roster::pending_injected_saves() != (0, 0) {
                    tokio::task::yield_now().await;
                }
                assert!(!worker.is_finished());
                {
                    let state = daemon.lock().await;
                    assert_eq!(
                        state.roster.sessions["session-a"].permission_mode,
                        Some(PermissionMode::Plan)
                    );
                    assert!(
                        state.roster.sessions["session-a"]
                            .guard_attempts
                            .contains_key(&recovery_token)
                    );
                }

                // The next retry refreshes fallback with S, then fails to
                // promote primary. That error also cannot roll memory back
                // or authorize the response.
                roster::fail_next_saves(1, 0);
                tokio::time::advance(FAIL_CLOSED_PERSIST_RETRY_MIN).await;
                while roster::pending_injected_saves().0 != 0 {
                    tokio::task::yield_now().await;
                }
                assert!(!worker.is_finished());
                assert!(gate.clone().try_lock_owned().is_err());
                let fallback: Roster = serde_json::from_slice(
                    &std::fs::read(
                        crate::rc::rc_dir()
                            .unwrap()
                            .join("sessions.fail-closed.json"),
                    )
                    .unwrap(),
                )
                .unwrap();
                assert_eq!(
                    fallback.sessions["session-a"].permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert!(
                    fallback.sessions["session-a"]
                        .guard_attempts
                        .contains_key(&recovery_token)
                );

                tokio::time::advance(FAIL_CLOSED_PERSIST_RETRY_MIN * 2).await;
                let executed = worker.await.unwrap();
                assert!(executed.response.is_err());
                assert!(gate.try_lock_owned().is_ok());
                let disk = Roster::try_load().unwrap();
                assert_eq!(
                    disk.sessions["session-a"].permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert!(
                    disk.sessions["session-a"]
                        .guard_attempts
                        .contains_key(&recovery_token)
                );
            });
    });
}

#[test]
fn a_late_typed_mode_unknown_never_sends_the_early_generic_reply() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let (tx, mut rx) = mpsc::channel(1);
                let live = rpc_test_live("session-a", 1, tx, PermissionMode::Auto);
                let gate = live.rpc_gate.clone();
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "session-a".into(),
                    rpc_test_roster_entry("session-a", PermissionMode::Auto),
                );
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
                let frame = rpc_mode_frame(PermissionMode::Plan);
                let request_id = frame.id.clone().unwrap();
                let prepared = daemon
                    .lock()
                    .await
                    .prepare_session_rpc(&frame)
                    .expect("mode change is admitted");
                let recovery_token = match &prepared.operation {
                    SessionRpcOperation::SetPermissionMode { recovery_token, .. } => {
                        recovery_token.clone()
                    }
                    _ => unreachable!(),
                };
                let (outbound, mut outbound_rx) = crate::rc::outbound::channel();
                let (_stop_tx, stop) = tokio::sync::watch::channel(false);
                let worker = tokio::spawn(prepared.serve(
                    daemon.clone(),
                    outbound,
                    request_id.clone(),
                    stop,
                ));
                let reply = match rx.recv().await.unwrap() {
                    Command::SetPermissionMode { reply, .. } => {
                        assert!(reply.accept());
                        reply
                    }
                    _ => panic!("wrong command"),
                };

                for _ in 0..2 {
                    tokio::time::advance(SESSION_REPLY_TIMEOUT).await;
                    tokio::task::yield_now().await;
                }
                assert!(
                    tokio::time::timeout(std::time::Duration::ZERO, outbound_rx.next_write(),)
                        .await
                        .is_err(),
                    "a taken sticky command cannot receive the generic early error"
                );
                assert!(gate.clone().try_lock_owned().is_err());

                reply.finish(Ok(PermissionModeOutcome::Unknown {
                    message: "late native ambiguity".into(),
                }));
                let sent = outbound_rx
                    .next_write()
                    .await
                    .expect("typed Unknown is answered after projection");
                assert_eq!(sent.frame().id.as_ref(), Some(&request_id));
                assert_eq!(
                    sent.frame().error.as_ref().unwrap().code,
                    ErrorCode::Internal as i32
                );
                sent.commit();
                worker.await.unwrap();
                assert!(gate.try_lock_owned().is_ok());
                let state = daemon.lock().await;
                assert!(
                    !state.sessions.contains_key("session-a"),
                    "the typed Unknown retires its generation before the reply is visible"
                );
                assert_eq!(
                    state.roster.sessions["session-a"].permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert!(
                    state.roster.sessions["session-a"]
                        .guard_attempts
                        .contains_key(&recovery_token)
                );
            });
    });
}

#[test]
fn a_taken_mode_command_with_a_lost_typed_outcome_keeps_its_gate() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let (tx, mut rx) = mpsc::channel(1);
                let live = rpc_test_live("session-a", 1, tx, PermissionMode::Auto);
                let gate = live.rpc_gate.clone();
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "session-a".into(),
                    rpc_test_roster_entry("session-a", PermissionMode::Auto),
                );
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
                let prepared = daemon
                    .lock()
                    .await
                    .prepare_session_rpc(&rpc_mode_frame(PermissionMode::Plan))
                    .unwrap();
                let worker_daemon = daemon.clone();
                let mut worker = tokio::spawn(async move {
                    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
                    prepared.execute(worker_daemon, &mut stop).await
                });
                match rx.recv().await.unwrap() {
                    Command::SetPermissionMode { reply, .. } => {
                        assert!(reply.accept());
                        drop(reply);
                    }
                    _ => panic!("wrong command"),
                }

                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(20), &mut worker)
                        .await
                        .is_err(),
                    "receipt closure is not process-tree termination proof"
                );
                assert!(gate.try_lock_owned().is_err());
                assert!(daemon.lock().await.sessions["session-a"].rpc_guard_sensitive);
                worker.abort();
                let _ = worker.await;
            });
    });
}

#[tokio::test]
async fn a_stale_mode_unknown_never_projects_into_a_replacement_generation() {
    use crate::protocol::PermissionMode;

    let (tx, _rx) = mpsc::channel(1);
    let mut roster = Roster::default();
    roster.sessions.insert(
        "session-a".into(),
        rpc_test_roster_entry("session-a", PermissionMode::Auto),
    );
    let daemon = rpc_test_daemon(
        [(
            "session-a".into(),
            rpc_test_live("session-a", 2, tx, PermissionMode::Auto),
        )]
        .into_iter()
        .collect(),
        roster,
    );
    let token = format!("{}old-generation", roster::SHUTDOWN_GUARD_PREFIX);
    let mut state = daemon.lock().await;
    state
        .complete_session_rpc(
            "session-a",
            1,
            &SessionRpcCompletion::PermissionMode {
                mode: PermissionMode::Bypass,
                applied: None,
                rollback_arm: None,
                recovery_token: Some(token),
                retire_generation: true,
            },
        )
        .expect("a stale completion is ignored");
    assert_eq!(
        state.sessions["session-a"].info.permission_mode,
        Some(PermissionMode::Auto)
    );
    assert_eq!(
        state.roster.sessions["session-a"].permission_mode,
        Some(PermissionMode::Auto)
    );
    assert!(!roster::has_shutdown_guard(
        &state.roster.sessions["session-a"].guard_attempts
    ));
}

#[test]
fn guard_sensitive_unknown_turn_persists_plan_before_returning_error() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let (tx, mut rx) = mpsc::channel(1);
                let mut live = rpc_test_live("session-a", 1, tx, PermissionMode::Auto);
                live.pending_mode = Some(PermissionMode::Bypass);
                let mut roster = Roster::default();
                let mut entry = rpc_test_roster_entry("session-a", PermissionMode::Auto);
                entry.ever_dangerous = true;
                roster.sessions.insert("session-a".into(), entry);
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
                let mut frame = Frame::request(
                    method::TURN_START,
                    TurnStart {
                        session_id: "session-a".into(),
                        message: "possibly accepted".into(),
                        by: Some("owner".into()),
                        client_msg_id: Some("message-1".into()),
                    },
                );
                frame.caller = Some(claim("owner", "ws-a"));
                let prepared = daemon.lock().await.prepare_session_rpc(&frame).unwrap();
                let worker_daemon = daemon.clone();
                let worker = tokio::spawn(async move {
                    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
                    prepared.execute(worker_daemon, &mut stop).await
                });
                match rx.recv().await.unwrap() {
                    Command::Turn { reply, .. } => {
                        assert!(reply.accept());
                        reply.finish(Ok(TurnStartOutcome::Unknown {
                            message: "native response was lost".into(),
                            attempted_mode: None,
                        }));
                    }
                    _ => panic!("wrong command"),
                }
                let executed = worker.await.unwrap();
                let error = executed.response.expect_err("unknown never succeeds");
                assert_ne!(error.code, ErrorCode::SessionBusy as i32);
                let state = daemon.lock().await;
                assert!(
                    !state.sessions.contains_key("session-a"),
                    "terminal turn receipt must retire Live without waiting for Ended"
                );
                assert_eq!(
                    state.roster.sessions["session-a"].permission_mode,
                    Some(PermissionMode::Plan),
                    "preflight guard sensitivity covers a lost attempted_mode"
                );
                drop(state);
                let persisted = Roster::load();
                assert_eq!(
                    persisted.sessions["session-a"].permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert!(persisted.sessions["session-a"].ever_dangerous);
            });
    });
}

#[tokio::test]
async fn terminal_turn_receipts_retire_live_before_the_ended_note() {
    use crate::protocol::PermissionMode;

    for outcome in [
        TurnStartOutcome::Unknown {
            message: "prompt consumption is unknown".into(),
            attempted_mode: None,
        },
        TurnStartOutcome::FatalNotAccepted {
            message: "native request ids are exhausted".into(),
        },
    ] {
        let (tx, mut rx) = mpsc::channel(1);
        let live = rpc_test_live("session-a", 1, tx, PermissionMode::Default);
        let daemon = rpc_test_daemon(
            [("session-a".into(), live)].into_iter().collect(),
            Roster::default(),
        );
        let prepared = daemon
            .lock()
            .await
            .prepare_session_rpc(&rpc_turn_frame("session-a"))
            .unwrap();
        let worker_daemon = daemon.clone();
        let worker = tokio::spawn(async move {
            let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
            prepared.execute(worker_daemon, &mut stop).await
        });
        match rx.recv().await.unwrap() {
            Command::Turn { reply, .. } => {
                assert!(reply.accept());
                reply.finish(Ok(outcome));
            }
            _ => panic!("wrong command"),
        }

        assert!(worker.await.unwrap().response.is_err());
        let mut state = daemon.lock().await;
        assert!(
            !state.sessions.contains_key("session-a"),
            "typed terminal outcome left a dead Live reusable"
        );
        state.on_session_note(SessionNote::Ended {
            session_id: "session-a".into(),
            generation: 1,
        });
        assert!(!state.sessions.contains_key("session-a"));
    }
}

#[tokio::test]
async fn one_shot_approval_unknown_retires_without_inventing_a_plan() {
    use crate::protocol::{ApprovalDecision, ApprovalResponse, ApprovalScope, PermissionMode};

    let (tx, mut rx) = mpsc::channel(1);
    let live = rpc_test_live("session-a", 1, tx, PermissionMode::Default);
    let daemon = rpc_test_daemon(
        [("session-a".into(), live)].into_iter().collect(),
        Roster::default(),
    );
    let mut frame = Frame::request(
        method::APPROVAL_DECIDE,
        ApprovalResponse {
            approval_id: "approval-1".into(),
            session_id: "session-a".into(),
            decision: ApprovalDecision::Allow,
            scope: ApprovalScope::Once,
            message: None,
            by: Some("owner".into()),
        },
    );
    frame.caller = Some(claim("owner", "ws-a"));
    let prepared = daemon.lock().await.prepare_session_rpc(&frame).unwrap();
    let worker_daemon = daemon.clone();
    let worker = tokio::spawn(async move {
        let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
        prepared.execute(worker_daemon, &mut stop).await
    });
    match rx.recv().await.unwrap() {
        Command::Approve { reply, .. } => {
            assert!(reply.accept());
            reply.finish(Ok(ApprovalOutcome::Unknown {
                message: "approval write may have landed".into(),
                attempted_mode: None,
            }));
        }
        _ => panic!("wrong command"),
    }

    let executed = worker.await.unwrap();
    assert_eq!(
        executed.response.unwrap_err().code,
        ErrorCode::Internal as i32
    );
    let mut state = daemon.lock().await;
    assert!(
        !state.sessions.contains_key("session-a"),
        "one-shot ambiguity must not leave a dead harness drivable"
    );
    assert!(
        state.roster.sessions.is_empty(),
        "one-shot ambiguity must not be mislabeled as a sticky Plan change"
    );
    state.on_session_note(SessionNote::Ended {
        session_id: "session-a".into(),
        generation: 1,
    });
    assert!(!state.sessions.contains_key("session-a"));
}

#[tokio::test(start_paused = true)]
async fn late_one_shot_approval_unknown_uses_the_same_generation_retirement() {
    use crate::protocol::{ApprovalDecision, ApprovalResponse, ApprovalScope, PermissionMode};

    let (tx, mut rx) = mpsc::channel(1);
    let live = rpc_test_live("session-a", 1, tx, PermissionMode::Default);
    let daemon = rpc_test_daemon(
        [("session-a".into(), live)].into_iter().collect(),
        Roster::default(),
    );
    let mut frame = Frame::request(
        method::APPROVAL_DECIDE,
        ApprovalResponse {
            approval_id: "approval-1".into(),
            session_id: "session-a".into(),
            decision: ApprovalDecision::Allow,
            scope: ApprovalScope::Once,
            message: None,
            by: Some("owner".into()),
        },
    );
    frame.caller = Some(claim("owner", "ws-a"));
    let prepared = daemon.lock().await.prepare_session_rpc(&frame).unwrap();
    let worker_daemon = daemon.clone();
    let worker = tokio::spawn(async move {
        let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
        prepared.execute(worker_daemon, &mut stop).await
    });
    let reply = match rx.recv().await.unwrap() {
        Command::Approve { reply, .. } => {
            assert!(reply.accept());
            reply
        }
        _ => panic!("wrong command"),
    };

    tokio::time::advance(SESSION_REPLY_TIMEOUT).await;
    tokio::task::yield_now().await;
    let executed = worker.await.unwrap();
    assert!(
        executed.response.is_err(),
        "the RPC has its bounded early reply"
    );
    let pending = executed
        .pending
        .expect("a taken one-shot approval keeps a late projector");
    assert!(
        daemon.lock().await.sessions.contains_key("session-a"),
        "no typed termination evidence exists yet"
    );

    reply.finish(Ok(ApprovalOutcome::Unknown {
        message: "late approval write ambiguity".into(),
        attempted_mode: None,
    }));
    pending.finish(daemon.clone()).await;
    let state = daemon.lock().await;
    assert!(
        !state.sessions.contains_key("session-a"),
        "the late projector must retire the same dead generation"
    );
    assert!(state.roster.sessions.is_empty());
}

#[test]
fn successful_session_approval_persists_its_trusted_mode_before_ack() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::{
                    ApprovalDecision, ApprovalResponse, ApprovalScope, PermissionMode,
                };

                let (tx, mut rx) = mpsc::channel(1);
                let mut live = rpc_test_live("session-a", 1, tx, PermissionMode::Default);
                live.info.runtime = "claude-code".into();
                live.approval_session_modes
                    .insert("approval-1".into(), PermissionMode::AcceptEdits);
                let gate = live.rpc_gate.clone();
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "session-a".into(),
                    rpc_test_roster_entry("session-a", PermissionMode::Default),
                );
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
                let mut frame = Frame::request(
                    method::APPROVAL_DECIDE,
                    ApprovalResponse {
                        approval_id: "approval-1".into(),
                        session_id: "session-a".into(),
                        decision: ApprovalDecision::Allow,
                        scope: ApprovalScope::Session,
                        message: None,
                        by: Some("owner".into()),
                    },
                );
                frame.caller = Some(claim("owner", "ws-a"));
                let prepared = daemon.lock().await.prepare_session_rpc(&frame).unwrap();
                let worker_daemon = daemon.clone();
                let worker = tokio::spawn(async move {
                    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
                    prepared.execute(worker_daemon, &mut stop).await
                });
                let reply = match rx.recv().await.unwrap() {
                    Command::Approve { reply, .. } => {
                        assert!(reply.accept());
                        reply
                    }
                    _ => panic!("wrong command"),
                };
                roster::fail_next_saves(1, 1);
                reply.finish(Ok(ApprovalOutcome::Applied {
                    effective_mode: Some(PermissionMode::AcceptEdits),
                }));
                while roster::pending_injected_saves() != (0, 0) {
                    tokio::task::yield_now().await;
                }
                assert!(!worker.is_finished(), "approval ACK waits for disk");
                assert!(gate.clone().try_lock_owned().is_err());

                tokio::time::advance(FAIL_CLOSED_PERSIST_RETRY_MIN).await;
                assert!(worker.await.unwrap().response.is_ok());
                assert!(gate.try_lock_owned().is_ok());
                let state = daemon.lock().await;
                assert_eq!(
                    state.sessions["session-a"].info.permission_mode,
                    Some(PermissionMode::AcceptEdits)
                );
                assert!(
                    !state.sessions["session-a"]
                        .approval_session_modes
                        .contains_key("approval-1")
                );
                drop(state);
                assert_eq!(
                    Roster::load().sessions["session-a"].permission_mode,
                    Some(PermissionMode::AcceptEdits)
                );
            });
    });
}

#[test]
fn explicit_session_approval_refusal_retains_the_card_and_rolls_back_its_arm() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::{
                    ApprovalDecision, ApprovalResponse, ApprovalScope, PermissionMode,
                };

                let (tx, mut rx) = mpsc::channel(1);
                let mut live = rpc_test_live("session-a", 1, tx, PermissionMode::Default);
                live.info.runtime = "claude-code".into();
                live.approval_session_modes
                    .insert("approval-1".into(), PermissionMode::Bypass);
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "session-a".into(),
                    rpc_test_roster_entry("session-a", PermissionMode::Default),
                );
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
                let mut frame = Frame::request(
                    method::APPROVAL_DECIDE,
                    ApprovalResponse {
                        approval_id: "approval-1".into(),
                        session_id: "session-a".into(),
                        decision: ApprovalDecision::Allow,
                        scope: ApprovalScope::Session,
                        message: None,
                        by: Some("owner".into()),
                    },
                );
                frame.caller = Some(claim("owner", "ws-a"));
                let prepared = daemon.lock().await.prepare_session_rpc(&frame).unwrap();
                assert!(daemon.lock().await.sessions["session-a"].info.dangerous);
                let worker_daemon = daemon.clone();
                let worker = tokio::spawn(async move {
                    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
                    prepared.execute(worker_daemon, &mut stop).await
                });
                let reply = match rx.recv().await.unwrap() {
                    Command::Approve { reply, .. } => {
                        assert!(reply.accept());
                        reply
                    }
                    _ => panic!("wrong command"),
                };
                reply.finish(Ok(ApprovalOutcome::ExplicitRefusal {
                    message: "only the owner may allow it".into(),
                    retained: true,
                }));

                let executed = worker.await.unwrap();
                assert_eq!(
                    executed.response.unwrap_err().code,
                    ErrorCode::ApprovalExpired as i32
                );
                let state = daemon.lock().await;
                let live = &state.sessions["session-a"];
                assert!(!live.info.dangerous);
                assert_eq!(live.info.permission_mode, Some(PermissionMode::Default));
                assert!(live.approval_session_modes.contains_key("approval-1"));
                drop(state);
                let persisted = Roster::load();
                assert!(!persisted.sessions["session-a"].ever_dangerous);
            });
    });
}

#[test]
fn unknown_session_approval_waits_for_late_receipt_and_durable_plan() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::{
                    ApprovalDecision, ApprovalResponse, ApprovalScope, PermissionMode,
                };

                let (tx, mut rx) = mpsc::channel(1);
                let mut live = rpc_test_live("session-a", 1, tx, PermissionMode::Default);
                live.info.runtime = "claude-code".into();
                live.approval_session_modes
                    .insert("approval-1".into(), PermissionMode::AcceptEdits);
                let gate = live.rpc_gate.clone();
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "session-a".into(),
                    rpc_test_roster_entry("session-a", PermissionMode::Default),
                );
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
                let mut frame = Frame::request(
                    method::APPROVAL_DECIDE,
                    ApprovalResponse {
                        approval_id: "approval-1".into(),
                        session_id: "session-a".into(),
                        decision: ApprovalDecision::Allow,
                        scope: ApprovalScope::Session,
                        message: None,
                        by: Some("owner".into()),
                    },
                );
                frame.caller = Some(claim("owner", "ws-a"));
                let prepared = daemon.lock().await.prepare_session_rpc(&frame).unwrap();
                let worker_daemon = daemon.clone();
                let worker = tokio::spawn(async move {
                    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
                    prepared.execute(worker_daemon, &mut stop).await
                });
                let reply = match rx.recv().await.unwrap() {
                    Command::Approve { reply, .. } => {
                        assert!(reply.accept());
                        reply
                    }
                    _ => panic!("wrong command"),
                };

                tokio::time::advance(SESSION_REPLY_TIMEOUT).await;
                tokio::task::yield_now().await;
                tokio::time::advance(SESSION_REPLY_TIMEOUT).await;
                tokio::task::yield_now().await;
                assert!(
                    !worker.is_finished(),
                    "session-policy approval must not return before its typed receipt"
                );
                assert!(gate.clone().try_lock_owned().is_err());

                roster::fail_next_saves(1, 1);
                reply.finish(Ok(ApprovalOutcome::Unknown {
                    message: "native write may have landed".into(),
                    attempted_mode: Some(PermissionMode::AcceptEdits),
                }));
                while roster::pending_injected_saves() != (0, 0) {
                    tokio::task::yield_now().await;
                }
                assert!(!worker.is_finished(), "no error ACK before durable Plan");
                assert!(daemon.try_lock().is_ok(), "retry frees the global mutex");
                assert!(gate.clone().try_lock_owned().is_err());
                assert_eq!(
                    daemon.lock().await.sessions["session-a"]
                        .info
                        .permission_mode,
                    Some(PermissionMode::Plan)
                );

                tokio::time::advance(FAIL_CLOSED_PERSIST_RETRY_MIN).await;
                let executed = worker.await.unwrap();
                assert_eq!(
                    executed.response.unwrap_err().code,
                    ErrorCode::Internal as i32
                );
                assert!(gate.try_lock_owned().is_ok());
                assert_eq!(
                    Roster::load().sessions["session-a"].permission_mode,
                    Some(PermissionMode::Plan)
                );
            });
    });
}

#[tokio::test]
async fn codex_session_scope_without_an_exact_mode_never_reaches_the_driver() {
    use crate::protocol::{ApprovalDecision, ApprovalResponse, ApprovalScope, PermissionMode};

    let (tx, mut rx) = mpsc::channel(1);
    let mut live = rpc_test_live("session-a", 1, tx, PermissionMode::Default);
    live.info.runtime = "codex".into();
    let daemon = rpc_test_daemon(
        [("session-a".into(), live)].into_iter().collect(),
        Roster::default(),
    );
    let mut frame = Frame::request(
        method::APPROVAL_DECIDE,
        ApprovalResponse {
            approval_id: "approval-1".into(),
            session_id: "session-a".into(),
            decision: ApprovalDecision::Allow,
            scope: ApprovalScope::Session,
            message: None,
            by: Some("owner".into()),
        },
    );
    frame.caller = Some(claim("owner", "ws-a"));

    let error = match daemon.lock().await.prepare_session_rpc(&frame) {
        Ok(_) => panic!("an unmodelled sticky policy must be refused locally"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::MalformedFrame as i32);
    assert!(error.message.contains("fully understood"));
    assert!(matches!(
        rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    let state = daemon.lock().await;
    assert!(!state.sessions["session-a"].rpc_guard_sensitive);
    assert!(!state.sessions["session-a"].info.dangerous);
}

#[test]
fn an_applied_approval_mode_must_match_the_trusted_suggestion_exactly() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::{
                    ApprovalDecision, ApprovalResponse, ApprovalScope, PermissionMode,
                };

                let (tx, mut rx) = mpsc::channel(1);
                let mut live = rpc_test_live("session-a", 1, tx, PermissionMode::Default);
                live.info.runtime = "claude-code".into();
                live.approval_session_modes
                    .insert("approval-1".into(), PermissionMode::AcceptEdits);
                let gate = live.rpc_gate.clone();
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "session-a".into(),
                    rpc_test_roster_entry("session-a", PermissionMode::Default),
                );
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
                let mut frame = Frame::request(
                    method::APPROVAL_DECIDE,
                    ApprovalResponse {
                        approval_id: "approval-1".into(),
                        session_id: "session-a".into(),
                        decision: ApprovalDecision::Allow,
                        scope: ApprovalScope::Session,
                        message: None,
                        by: Some("owner".into()),
                    },
                );
                frame.caller = Some(claim("owner", "ws-a"));
                let prepared = daemon.lock().await.prepare_session_rpc(&frame).unwrap();
                let worker_daemon = daemon.clone();
                let worker = tokio::spawn(async move {
                    let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
                    prepared.execute(worker_daemon, &mut stop).await
                });
                let reply = match rx.recv().await.unwrap() {
                    Command::Approve { reply, .. } => {
                        assert!(reply.accept());
                        reply
                    }
                    _ => panic!("wrong command"),
                };
                reply.finish(Ok(ApprovalOutcome::Applied {
                    effective_mode: Some(PermissionMode::Auto),
                }));
                tokio::task::yield_now().await;

                assert!(
                    !worker.is_finished(),
                    "a mismatch must never receive an ACK"
                );
                assert!(gate.clone().try_lock_owned().is_err());
                assert_eq!(
                    daemon.lock().await.sessions["session-a"]
                        .info
                        .permission_mode,
                    Some(PermissionMode::Default)
                );
                worker.abort();
                let _ = worker.await;
                assert!(gate.try_lock_owned().is_ok());
            });
    });
}
