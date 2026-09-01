use super::*;

#[tokio::test]
async fn guard_sensitive_commands_defer_until_a_durable_bound_row() {
    use crate::protocol::{ApprovalDecision, ApprovalResponse, ApprovalScope, PermissionMode};

    let (mode_tx, mut mode_rx) = mpsc::channel(1);
    let mode_daemon = rpc_test_daemon(
        [(
            "session-a".into(),
            rpc_test_live("session-a", 1, mode_tx, PermissionMode::Auto),
        )]
        .into_iter()
        .collect(),
        Roster::default(),
    );
    let mode_lease = match mode_daemon
        .lock()
        .await
        .prepare_session_rpc_or_wait(&rpc_mode_frame(PermissionMode::Plan))
    {
        Ok(SessionRpcPreparation::AwaitingDurableGuardRow(lease)) => lease,
        Ok(SessionRpcPreparation::Ready(_)) => {
            panic!("set-mode reached the queue before Bound was durable")
        }
        Err(error) => panic!("the transient binding window was rejected: {error:?}"),
    };
    assert!(matches!(
        mode_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(!mode_daemon.lock().await.sessions["session-a"].rpc_guard_sensitive);
    drop(mode_lease);

    let (approval_tx, mut approval_rx) = mpsc::channel(1);
    let mut approval_live = rpc_test_live("session-a", 1, approval_tx, PermissionMode::Default);
    approval_live.info.runtime = "claude-code".into();
    approval_live
        .approval_session_modes
        .insert("approval-1".into(), PermissionMode::AcceptEdits);
    let approval_daemon = rpc_test_daemon(
        [("session-a".into(), approval_live)].into_iter().collect(),
        Roster::default(),
    );
    let mut approval = Frame::request(
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
    approval.caller = Some(claim("owner", "ws-a"));
    let approval_lease = match approval_daemon
        .lock()
        .await
        .prepare_session_rpc_or_wait(&approval)
    {
        Ok(SessionRpcPreparation::AwaitingDurableGuardRow(lease)) => lease,
        Ok(SessionRpcPreparation::Ready(_)) => {
            panic!("sticky approval reached the queue before Bound was durable")
        }
        Err(error) => panic!("the transient binding window was rejected: {error:?}"),
    };
    assert!(matches!(
        approval_rx.try_recv(),
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
    ));
    assert!(!approval_daemon.lock().await.sessions["session-a"].rpc_guard_sensitive);
    drop(approval_lease);
}

/// The browser commonly sends a Codex mode change immediately after
/// `session.start`, before native Ready/Bound. That transient window must
/// be absorbed on the machine: keep per-session ordering, release the
/// daemon mutex so Bound can land, then prepare against fresh authority.
#[test]
fn durable_guard_bind_wait_preserves_the_complete_hub_rpc_window() {
    // A queued RPC can spend one window reserving queue capacity, one waiting for the
    // executor to accept its ticket, and one more waiting for an accepted result. Counting
    // only the last phase understates the ordinary machine path by two whole windows, which
    // is exactly how a relay deadline too small to hold it can still look safe.
    let consumed = DURABLE_GUARD_BIND_TIMEOUT + SESSION_REPLY_TIMEOUT * 3;
    assert!(consumed < HUB_RELAY_TIMEOUT);
    assert_eq!(
        HUB_RELAY_TIMEOUT - consumed,
        std::time::Duration::from_secs(9),
        "the full queued-RPC path still needs explicit relay/network headroom"
    );
}

#[test]
fn an_immediate_mode_change_waits_through_slow_bound_without_the_daemon_mutex() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::{PermissionApply, PermissionMode};

                let (tx, mut rx) = mpsc::channel(2);
                let live = rpc_test_live("session-a", 1, tx, PermissionMode::Auto);
                let gate = live.rpc_gate.clone();
                let daemon = rpc_test_daemon(
                    [("session-a".into(), live)].into_iter().collect(),
                    Roster::default(),
                );
                let frame = rpc_mode_frame(PermissionMode::Plan);
                let request_id = frame.id.clone().unwrap();
                let connection_epoch = daemon.lock().await.settlement.borrow().epoch;
                let lease = match daemon
                    .lock()
                    .await
                    .prepare_session_rpc_or_wait(&frame)
                    .unwrap()
                {
                    SessionRpcPreparation::AwaitingDurableGuardRow(lease) => lease,
                    SessionRpcPreparation::Ready(_) => panic!("Bound has not happened yet"),
                };
                let (outbound, mut outbound_rx) = crate::rc::outbound::channel();
                let (_stop_tx, stop) = tokio::sync::watch::channel(false);
                let worker = tokio::spawn(lease.serve_when_bound(SessionRpcBoundWait {
                    daemon: daemon.clone(),
                    outbound,
                    id: request_id.clone(),
                    frame,
                    connection_epoch,
                    stop,
                }));

                // This is the deadlock regression: the waiter may own the
                // session gate, but Bound arrives through the global
                // daemon mutex and must be able to acquire it promptly.
                let state =
                    tokio::time::timeout(std::time::Duration::from_millis(200), daemon.lock())
                        .await
                        .expect("the Bound waiter does not hold the daemon mutex");
                drop(state);
                assert!(
                    gate.clone().try_lock_owned().is_err(),
                    "the first request retains per-session ordering while it waits"
                );
                let overtaking = match daemon
                    .lock()
                    .await
                    .prepare_session_rpc_or_wait(&rpc_mode_frame(PermissionMode::Default))
                {
                    Err(error) => error,
                    Ok(_) => panic!("a later mode change overtook the waiter"),
                };
                assert_eq!(overtaking.code, ErrorCode::SessionBusy as i32);

                // The Codex app-server fallback can reach `Bound` later than the ordinary
                // receipt window allows, so a waiter that reuses that window answers a
                // false 303 just before the authoritative note arrives.
                tokio::time::advance(SESSION_REPLY_TIMEOUT + std::time::Duration::from_secs(1))
                    .await;
                tokio::task::yield_now().await;

                daemon.lock().await.on_session_note(SessionNote::Bound {
                    session_id: "session-a".into(),
                    generation: 1,
                    runtime_thread_id: "native-codex-thread".into(),
                    cwd: home.path().to_string_lossy().into_owned(),
                    agit_session: None,
                    expected_agent_id: None,
                });
                let reply = match tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                    .await
                    .expect("the waiter notices Bound")
                    .expect("the supervisor channel stays open")
                {
                    Command::SetPermissionMode { mode, reply, .. } => {
                        assert_eq!(mode, PermissionMode::Plan);
                        assert!(reply.accept());
                        reply
                    }
                    _ => panic!("wrong command"),
                };
                reply.finish(Ok(PermissionModeOutcome::Applied {
                    applied: PermissionApply::Immediate,
                }));

                let sent = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    outbound_rx.next_write(),
                )
                .await
                .expect("the browser receives the delayed result")
                .expect("the reply lane stays open");
                assert_eq!(sent.frame().id.as_ref(), Some(&request_id));
                assert!(sent.frame().error.is_none(), "{:?}", sent.frame().error);
                sent.commit();
                worker.await.unwrap();
                assert!(gate.try_lock_owned().is_ok());
                assert_eq!(
                    daemon.lock().await.roster.sessions["session-a"].thread_id,
                    "native-codex-thread"
                );
            });
    });
}

#[test]
fn a_bound_wait_timeout_never_queues_the_guard_sensitive_command() {
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
            let daemon = rpc_test_daemon(
                [("session-a".into(), live)].into_iter().collect(),
                Roster::default(),
            );
            let frame = rpc_mode_frame(PermissionMode::Plan);
            let request_id = frame.id.clone().unwrap();
            let connection_epoch = daemon.lock().await.settlement.borrow().epoch;
            let lease = match daemon
                .lock()
                .await
                .prepare_session_rpc_or_wait(&frame)
                .unwrap()
            {
                SessionRpcPreparation::AwaitingDurableGuardRow(lease) => lease,
                SessionRpcPreparation::Ready(_) => panic!("Bound has not happened yet"),
            };
            let (outbound, mut outbound_rx) = crate::rc::outbound::channel();
            let (_stop_tx, stop) = tokio::sync::watch::channel(false);
            let worker = tokio::spawn(lease.serve_when_bound_within(
                SessionRpcBoundWait {
                    daemon,
                    outbound,
                    id: request_id,
                    frame,
                    connection_epoch,
                    stop,
                },
                std::time::Duration::from_millis(100),
            ));

            tokio::time::advance(std::time::Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
            let sent = outbound_rx.next_write().await.expect("timeout reply");
            assert_eq!(
                sent.frame().error.as_ref().unwrap().code,
                ErrorCode::SessionBusy as i32
            );
            sent.commit();
            worker.await.unwrap();
            assert!(
                rx.try_recv().is_err(),
                "the timed-out request must never reach the supervisor queue"
            );
            assert!(gate.try_lock_owned().is_ok());
        });
}

#[test]
fn an_ended_session_is_removed_after_its_bound_wait_releases_the_gate() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            use crate::protocol::PermissionMode;

            let (tx, _rx) = mpsc::channel(1);
            let live = rpc_test_live("session-a", 1, tx, PermissionMode::Auto);
            let daemon = rpc_test_daemon(
                [("session-a".into(), live)].into_iter().collect(),
                Roster::default(),
            );
            let frame = rpc_mode_frame(PermissionMode::Plan);
            let request_id = frame.id.clone().unwrap();
            let connection_epoch = daemon.lock().await.settlement.borrow().epoch;
            let lease = match daemon
                .lock()
                .await
                .prepare_session_rpc_or_wait(&frame)
                .unwrap()
            {
                SessionRpcPreparation::AwaitingDurableGuardRow(lease) => lease,
                SessionRpcPreparation::Ready(_) => panic!("Bound has not happened yet"),
            };
            let (outbound, mut outbound_rx) = crate::rc::outbound::channel();
            let (_stop_tx, stop) = tokio::sync::watch::channel(false);
            let worker = tokio::spawn(lease.serve_when_bound(SessionRpcBoundWait {
                daemon: daemon.clone(),
                outbound,
                id: request_id,
                frame,
                connection_epoch,
                stop,
            }));

            daemon.lock().await.on_session_note(SessionNote::Ended {
                session_id: "session-a".into(),
                generation: 1,
            });
            let sent =
                tokio::time::timeout(std::time::Duration::from_secs(1), outbound_rx.next_write())
                    .await
                    .expect("the waiter observes the tombstone")
                    .expect("error reply");
            assert_eq!(
                sent.frame().error.as_ref().unwrap().code,
                ErrorCode::SessionNotFound as i32
            );
            sent.commit();
            worker.await.unwrap();
            assert!(
                !daemon.lock().await.sessions.contains_key("session-a"),
                "releasing an unprepared gate must finish deferred Ended cleanup"
            );
        });
}

#[test]
fn only_claude_with_an_inherited_guard_uses_the_internal_ready_barrier() {
    let inherited = std::collections::BTreeSet::from(["turn-guard".to_string()]);
    assert!(needs_claude_restart_guard_barrier(
        "claude-code",
        &inherited
    ));
    assert!(
        !needs_claude_restart_guard_barrier("codex", &inherited),
        "Codex must continue waiting for its native Ready evidence"
    );
    assert!(!needs_claude_restart_guard_barrier(
        "claude-code",
        &Default::default()
    ));
}

#[tokio::test]
async fn failed_bootstrap_detaches_a_supervisor_blocked_on_a_full_notes_channel() {
    let (command_tx, _command_rx) = mpsc::channel(1);
    let (note_tx, mut note_rx) = mpsc::channel(1);
    note_tx.send(()).await.expect("fill notes channel");
    let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let blocked_finished = finished.clone();
    let blocked_note_tx = note_tx.clone();
    let blocked_task = tokio::spawn(async move {
        blocked_note_tx
            .send(())
            .await
            .expect("notes receiver remains alive");
        blocked_finished.store(true, std::sync::atomic::Ordering::SeqCst);
    });
    tokio::task::yield_now().await;
    assert!(
        !finished.load(std::sync::atomic::Ordering::SeqCst),
        "fixture task must be parked behind the full notes channel"
    );

    let mut live = rpc_test_live(
        "session-a",
        7,
        command_tx,
        crate::protocol::PermissionMode::Plan,
    );
    live.task.abort();
    live.task = blocked_task;
    let daemon = rpc_test_daemon(
        [("session-a".to_string(), live)].into_iter().collect(),
        Roster::default(),
    );
    {
        let mut state = daemon.lock().await;
        state
            .latest_session_generations
            .insert("session-a".into(), 7);
        state.journal.record(
            "session-a",
            Frame::notification("item.started", serde_json::json!({})),
        );
        state.journal.resume("session-a");
        state.detach_failed_session_generation("session-a", 7);
        assert!(!state.sessions.contains_key("session-a"));
        assert_eq!(
            state.latest_session_generations["session-a"], 7,
            "detach removes only Live; its materialization tombstone is permanent"
        );
        state.journal.record(
            "session-a",
            Frame::notification("item.completed", serde_json::json!({})),
        );
        assert!(
            state.journal.replay("session-a", 0).0.is_empty(),
            "failed spawn must leave its replay stream forgotten"
        );
    }
    assert!(
        !finished.load(std::sync::atomic::Ordering::SeqCst),
        "cleanup must not wait for a supervisor that needs the notes pump"
    );

    note_rx.recv().await.expect("drain the blocking note");
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while !finished.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached reaper lets the supervisor finish after notes drain");
}

#[test]
fn bound_preserves_all_guard_tokens_and_applies_the_shutdown_floor() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let (tx, _rx) = mpsc::channel(1);
                let mut live = rpc_test_live("session-a", 2, tx, PermissionMode::Bypass);
                live.pending_mode = Some(PermissionMode::Auto);
                let shutdown = hard_stop_guard("session-a", 2, "bound-floor");
                let mut entry = rpc_test_roster_entry("session-a", PermissionMode::Bypass);
                for (token, mode, observed) in [
                    ("turn-a".to_string(), PermissionMode::Bypass, true),
                    ("turn-b".to_string(), PermissionMode::Auto, false),
                    (shutdown.token.clone(), PermissionMode::Plan, false),
                ] {
                    entry.guard_attempts.insert(
                        token,
                        roster::GuardAttempt {
                            expected_mode: mode,
                            observed,
                        },
                    );
                }
                let mut roster = Roster::default();
                roster.sessions.insert("session-a".into(), entry);
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);

                let bound = |generation, thread: &str| SessionNote::Bound {
                    session_id: "session-a".into(),
                    generation,
                    runtime_thread_id: thread.into(),
                    cwd: "/tmp/project".into(),
                    agit_session: None,
                    expected_agent_id: None,
                };
                let mut state = daemon.lock().await;
                state.on_session_note(bound(1, "stale-thread"));
                assert_ne!(state.roster.sessions["session-a"].thread_id, "stale-thread");

                state.on_session_note(bound(2, "current-thread"));
                let live = &state.sessions["session-a"];
                assert_eq!(live.runtime_thread_id.as_deref(), Some("current-thread"));
                assert_eq!(live.info.permission_mode, Some(PermissionMode::Plan));
                assert_eq!(live.pending_mode, None);
                let entry = &state.roster.sessions["session-a"];
                assert_eq!(entry.thread_id, "current-thread");
                assert_eq!(entry.permission_mode, Some(PermissionMode::Plan));
                assert!(entry.guard_attempts.contains_key("turn-a"));
                assert!(entry.guard_attempts.contains_key("turn-b"));
                assert!(entry.guard_attempts.contains_key(&shutdown.token));
                drop(state);

                let disk = Roster::load();
                assert_eq!(
                    disk.sessions["session-a"].permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert_eq!(disk.sessions["session-a"].guard_attempts.len(), 3);
            });
    });
}

/// When the `save()` for that `Bound` fails, the poison "the current thread binding is not durable
/// yet" **must not be released in memory first**.
///
/// claude's slow-path recovery swaps in a new native id at `system/init`, and the transcript file
/// follows. `Bound` arrives with the new id and `record` writes it into memory, but `save()` fails:
/// disk still holds the old id, so the new transcript — carrying everything this unchecked
/// run read into its context — is still unowned as far as the ledger is concerned. Striking the
/// poison out here lets any operator, for as long as this daemon lives, take it over under the
/// new id and mint a clean `ever_dangerous == false` identity.
#[test]
fn a_failed_bound_save_keeps_the_rotated_thread_locked_to_the_owner() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let (tx, _rx) = mpsc::channel(2);
                let mut live = rpc_test_live("agit-x", 1, tx, PermissionMode::Bypass);
                live.info.runtime = "claude-code".into();
                live.info.dangerous = true;

                let mut entry = rpc_test_roster_entry("agit-x", PermissionMode::Bypass);
                entry.runtime = "claude-code".into();
                entry.thread_id = "t-old".into();
                entry.cwd = "/srv/app".into();
                entry.workspace_id = "ws-a".into();
                entry.ever_dangerous = true;
                let mut roster = Roster::default();
                roster.sessions.insert("agit-x".into(), entry);
                // The one `spawn_session` arms before launch.
                assert!(roster.arm_unconfirmed_binding("agit-x"));
                roster.save().unwrap();

                let daemon =
                    rpc_test_daemon([("agit-x".into(), live)].into_iter().collect(), roster);
                let mut state = daemon.lock().await;

                let bound = |thread: &str| SessionNote::Bound {
                    session_id: "agit-x".into(),
                    generation: 1,
                    runtime_thread_id: thread.into(),
                    cwd: "/srv/app".into(),
                    agit_session: None,
                    expected_agent_id: None,
                };

                // `Roster::save` takes the main ledger path and does not touch the fail-closed
                // backup.
                roster::fail_next_saves(1, 0);
                state.on_session_note(bound("t-new"));
                assert_eq!(roster::pending_injected_saves(), (0, 0));
                assert!(
                    // A thread the ledger **cannot recognize**: the poison is still on, so on this
                    // territory it is dangerous; the test is `transcript_ever_dangerous` alone.
                    state.roster.transcript_ever_dangerous(
                        "claude-code",
                        "t-rotated-again",
                        "ws-a",
                        "/srv/app"
                    ),
                    "an unpersisted new id keeps an unrecognized thread on this territory dangerous"
                );
                assert!(
                    state.roster.transcript_ever_dangerous(
                        "claude-code",
                        "t-new",
                        "ws-a",
                        "/srv/app"
                    ),
                    "a takeover under the new id picks up the context of that unchecked run"
                );

                // The next `Bound` (resent before every turn's settlement) retries it.
                state.on_session_note(bound("t-new"));
                assert!(
                    !state.roster.transcript_ever_dangerous(
                        "claude-code",
                        "t-rotated-again",
                        "ws-a",
                        "/srv/app"
                    ),
                    "the poison lifts only once the binding is durable"
                );
                drop(state);
                let disk = Roster::load();
                assert_eq!(disk.sessions["agit-x"].thread_id, "t-new");
                assert!(disk.unconfirmed_dangerous_bindings.is_empty());
            });
    });
}

#[test]
fn restart_ready_clears_only_its_spawn_snapshot_after_a_durable_save() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let (tx, _rx) = mpsc::channel(2);
                let mut live = rpc_test_live("session-a", 7, tx, PermissionMode::Plan);
                let inherited = hard_stop_guard("session-a", 6, "inherited");
                let same_generation = hard_stop_guard("session-a", 7, "same-generation");
                live.restart_guard_attempts.insert(inherited.token.clone());
                live.restart_guard_mode = Some(PermissionMode::Plan);
                let mut entry = rpc_test_roster_entry("session-a", PermissionMode::Auto);
                entry.guard_attempts.insert(
                    inherited.token.clone(),
                    roster::GuardAttempt {
                        expected_mode: PermissionMode::Plan,
                        observed: false,
                    },
                );
                entry.guard_attempts.insert(
                    same_generation.token.clone(),
                    roster::GuardAttempt {
                        expected_mode: PermissionMode::Plan,
                        observed: false,
                    },
                );
                let mut roster = Roster::default();
                roster.sessions.insert("session-a".into(), entry);
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);

                let stale = daemon
                    .lock()
                    .await
                    .clear_restart_guards_after_ready("session-a", 6)
                    .expect_err("an old generation cannot clear a new launch barrier");
                assert!(stale.to_string().contains("generation disappeared"));

                let blocked = match daemon
                    .lock()
                    .await
                    .prepare_session_rpc(&rpc_turn_frame("session-a"))
                {
                    Ok(_) => panic!("Drive crossed the restart Ready barrier"),
                    Err(error) => error,
                };
                assert_eq!(blocked.code, ErrorCode::SessionBusy as i32);

                let mut interrupt = Frame::request(
                    method::TURN_INTERRUPT,
                    TurnInterrupt {
                        session_id: "session-a".into(),
                        by: Some("operator".into()),
                    },
                );
                interrupt.caller = Some(claim("operator", "ws-a"));
                drop(
                    daemon
                        .lock()
                        .await
                        .prepare_session_rpc(&interrupt)
                        .expect("a brake remains available before Ready"),
                );

                roster::fail_next_saves(1, 1);
                assert!(
                    daemon
                        .lock()
                        .await
                        .clear_restart_guards_after_ready("session-a", 7)
                        .is_err(),
                    "neither failed destination authorizes the Ready ACK"
                );
                {
                    let state = daemon.lock().await;
                    let live = &state.sessions["session-a"];
                    assert!(live.restart_guard_attempts.contains(&inherited.token));
                    assert_eq!(live.restart_guard_mode, Some(PermissionMode::Plan));
                    let entry = &state.roster.sessions["session-a"];
                    assert!(entry.guard_attempts.contains_key(&inherited.token));
                    assert!(entry.guard_attempts.contains_key(&same_generation.token));
                    assert_eq!(entry.permission_mode, Some(PermissionMode::Auto));
                }
                let disk = Roster::load();
                assert!(
                    disk.sessions["session-a"]
                        .guard_attempts
                        .contains_key(&inherited.token)
                );

                daemon
                    .lock()
                    .await
                    .clear_restart_guards_after_ready("session-a", 7)
                    .expect("retry durably promotes the launch mode");
                {
                    let state = daemon.lock().await;
                    let live = &state.sessions["session-a"];
                    assert!(live.restart_guard_attempts.is_empty());
                    assert_eq!(live.restart_guard_mode, None);
                    assert_eq!(live.info.permission_mode, Some(PermissionMode::Plan));
                    let entry = &state.roster.sessions["session-a"];
                    assert!(!entry.guard_attempts.contains_key(&inherited.token));
                    assert!(
                        entry.guard_attempts.contains_key(&same_generation.token),
                        "Ready clears only the spawn snapshot"
                    );
                    assert_eq!(entry.permission_mode, Some(PermissionMode::Plan));
                }
                let disk = Roster::load();
                assert!(
                    !disk.sessions["session-a"]
                        .guard_attempts
                        .contains_key(&inherited.token)
                );
                assert!(
                    disk.sessions["session-a"]
                        .guard_attempts
                        .contains_key(&same_generation.token)
                );
                assert_eq!(
                    disk.sessions["session-a"].restart_permission_mode(),
                    Some(PermissionMode::Plan)
                );
            });
    });
}

#[test]
fn prearm_failure_never_enqueues_or_leaves_an_in_memory_token() {
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
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "session-a".into(),
                    rpc_test_roster_entry("session-a", PermissionMode::Auto),
                );
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);

                roster::fail_next_saves(1, 1);
                let error = match daemon
                    .lock()
                    .await
                    .prepare_session_rpc(&rpc_turn_frame("session-a"))
                {
                    Ok(_) => panic!("an undurable guard attempt reached execution"),
                    Err(error) => error,
                };
                assert_eq!(error.code, ErrorCode::Internal as i32);
                assert!(rx.try_recv().is_err(), "no Command reached the supervisor");
                let state = daemon.lock().await;
                assert!(state.roster.sessions["session-a"].guard_attempts.is_empty());
                assert_eq!(state.sessions["session-a"].inflight_turn_guard, None);
                assert!(!state.sessions["session-a"].rpc_guard_sensitive);
                drop(state);
                assert!(
                    Roster::load().sessions["session-a"]
                        .guard_attempts
                        .is_empty()
                );
            });
    });
}

#[test]
fn late_confirmation_clears_only_its_token_and_never_rearms_on_rpc_completion() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let (tx, _rx) = mpsc::channel(1);
                let mut live = rpc_test_live("session-a", 1, tx, PermissionMode::Auto);
                live.pending_mode = Some(PermissionMode::Bypass);
                let mut roster = Roster::default();
                let mut entry = rpc_test_roster_entry("session-a", PermissionMode::Auto);
                entry.guard_attempts.insert(
                    "newer".into(),
                    roster::GuardAttempt {
                        expected_mode: PermissionMode::Auto,
                        observed: false,
                    },
                );
                let shutdown = hard_stop_guard("session-a", 1, "late-exact-floor");
                entry.guard_attempts.insert(
                    shutdown.token.clone(),
                    roster::GuardAttempt {
                        expected_mode: PermissionMode::Plan,
                        observed: false,
                    },
                );
                roster.sessions.insert("session-a".into(), entry);
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);

                let attempt = daemon
                    .lock()
                    .await
                    .prearm_turn_guard_attempt("session-a")
                    .unwrap()
                    .unwrap();
                let synthetic = daemon
                    .lock()
                    .await
                    .confirm_turn_guard("session-a", 1, &shutdown.token)
                    .expect_err("native exact evidence cannot name synthetic S");
                assert!(synthetic.to_string().contains("shutdown guard token"));
                daemon
                    .lock()
                    .await
                    .observe_turn_guard("session-a", 1, &attempt.token)
                    .unwrap();
                roster::fail_next_saves(1, 1);
                assert!(
                    daemon
                        .lock()
                        .await
                        .confirm_turn_guard("session-a", 1, &attempt.token)
                        .is_err(),
                    "late exact cannot ACK until its removal is durable"
                );
                {
                    let state = daemon.lock().await;
                    let entry = &state.roster.sessions["session-a"];
                    assert!(entry.guard_attempts.contains_key(&attempt.token));
                    assert!(entry.guard_attempts.contains_key(&shutdown.token));
                }
                daemon
                    .lock()
                    .await
                    .confirm_turn_guard("session-a", 1, &attempt.token)
                    .unwrap();
                {
                    let state = daemon.lock().await;
                    assert_eq!(
                        state.sessions["session-a"].info.permission_mode,
                        Some(PermissionMode::Plan),
                        "observing real token A cannot cross synthetic floor S"
                    );
                    assert!(state.sessions["session-a"].info.dangerous);
                    assert!(
                        !state.roster.sessions["session-a"]
                            .guard_attempts
                            .contains_key(&attempt.token)
                    );
                    assert!(
                        state.roster.sessions["session-a"]
                            .guard_attempts
                            .contains_key("newer")
                    );
                    assert!(
                        state.roster.sessions["session-a"]
                            .guard_attempts
                            .contains_key(&shutdown.token)
                    );
                    assert_eq!(
                        state.sessions["session-a"]
                            .confirmed_turn_guards
                            .get(&attempt.token),
                        Some(&PermissionMode::Bypass)
                    );
                }

                daemon
                    .lock()
                    .await
                    .complete_session_rpc(
                        "session-a",
                        1,
                        &SessionRpcCompletion::Turn {
                            guard_attempt: Some(attempt.clone()),
                            accepted_mode: Some(PermissionMode::Bypass),
                            confirmation: Some(TurnStartConfirmation::NotificationOnly),
                            fail_closed: false,
                            retire_generation: false,
                        },
                    )
                    .unwrap();
                let state = daemon.lock().await;
                assert!(
                    !state.sessions["session-a"]
                        .confirmed_turn_guards
                        .contains_key(&attempt.token)
                );
                assert_eq!(state.sessions["session-a"].inflight_turn_guard, None);
                assert!(
                    !state.roster.sessions["session-a"]
                        .guard_attempts
                        .contains_key(&attempt.token)
                );
                assert!(
                    state.roster.sessions["session-a"]
                        .guard_attempts
                        .contains_key("newer")
                );
                assert!(
                    state.roster.sessions["session-a"]
                        .guard_attempts
                        .contains_key(&shutdown.token)
                );
                assert_eq!(
                    state.sessions["session-a"].info.permission_mode,
                    Some(PermissionMode::Plan)
                );
                drop(state);

                let disk = Roster::load();
                assert!(
                    disk.sessions["session-a"]
                        .guard_attempts
                        .contains_key(&shutdown.token)
                );

                let missing = daemon
                    .lock()
                    .await
                    .confirm_turn_guard("session-a", 1, "never-armed")
                    .expect_err("a missing token is not confirmation evidence");
                assert!(missing.to_string().contains("neither armed"));
            });
    });
}

#[test]
fn accepted_mode_mismatches_are_projected_fail_closed_in_both_directions() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let attempt = crate::rc::harness::TurnGuardAttempt {
                    token: "guarded".into(),
                    expected_mode: PermissionMode::Plan,
                };
                let (completion, response) = project_turn_start_outcome(
                    TurnStartOutcome::Accepted {
                        turn_id: "turn-1".into(),
                        still_running: true,
                        consumed_mode: None,
                        confirmation: TurnStartConfirmation::Exact,
                    },
                    Some(attempt.clone()),
                );
                assert!(response.is_err());
                assert!(matches!(
                    completion,
                    SessionRpcCompletion::Turn {
                        fail_closed: true,
                        ..
                    }
                ));

                let (tx, _rx) = mpsc::channel(1);
                let mut live = rpc_test_live("session-a", 1, tx, PermissionMode::Auto);
                live.inflight_turn_guard = Some(attempt.token.clone());
                let mut entry = rpc_test_roster_entry("session-a", PermissionMode::Auto);
                entry.guard_attempts.insert(
                    attempt.token.clone(),
                    roster::GuardAttempt {
                        expected_mode: PermissionMode::Plan,
                        observed: false,
                    },
                );
                let mut roster = Roster::default();
                roster.sessions.insert("session-a".into(), entry);
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
                daemon
                    .lock()
                    .await
                    .complete_session_rpc("session-a", 1, &completion)
                    .unwrap();
                {
                    let state = daemon.lock().await;
                    assert_eq!(
                        state.sessions["session-a"].info.permission_mode,
                        Some(PermissionMode::Plan)
                    );
                    assert!(
                        state.roster.sessions["session-a"]
                            .guard_attempts
                            .contains_key(&attempt.token)
                    );
                }

                let (reverse, response) = project_turn_start_outcome(
                    TurnStartOutcome::Accepted {
                        turn_id: "turn-2".into(),
                        still_running: true,
                        consumed_mode: Some(PermissionMode::Bypass),
                        confirmation: TurnStartConfirmation::Exact,
                    },
                    None,
                );
                assert!(response.is_err());
                assert!(matches!(
                    reverse,
                    SessionRpcCompletion::Turn {
                        guard_attempt: None,
                        fail_closed: true,
                        ..
                    }
                ));
            });
    });
}

#[test]
fn fatal_prewrite_turn_exhaustion_clears_its_guard_without_persisting_plan() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let attempt = crate::rc::harness::TurnGuardAttempt {
                    token: "fatal-prewrite".into(),
                    expected_mode: PermissionMode::Bypass,
                };
                let (completion, response) = project_turn_start_outcome(
                    TurnStartOutcome::FatalNotAccepted {
                        message: "request-id space exhausted".into(),
                    },
                    Some(attempt.clone()),
                );
                assert!(response.is_err());
                assert!(matches!(
                    completion,
                    SessionRpcCompletion::Turn {
                        accepted_mode: None,
                        confirmation: None,
                        fail_closed: false,
                        ..
                    }
                ));

                let (tx, _rx) = mpsc::channel(1);
                let mut live = rpc_test_live("session-a", 1, tx, PermissionMode::Auto);
                live.pending_mode = Some(PermissionMode::Bypass);
                live.inflight_turn_guard = Some(attempt.token.clone());
                let mut entry = rpc_test_roster_entry("session-a", PermissionMode::Auto);
                entry.guard_attempts.insert(
                    attempt.token.clone(),
                    roster::GuardAttempt {
                        expected_mode: PermissionMode::Bypass,
                        observed: false,
                    },
                );
                let mut roster = Roster::default();
                roster.sessions.insert("session-a".into(), entry);
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
                daemon
                    .lock()
                    .await
                    .complete_session_rpc("session-a", 1, &completion)
                    .unwrap();

                let state = daemon.lock().await;
                assert!(
                    !state.sessions.contains_key("session-a"),
                    "fatal allocator exhaustion retires the unusable generation"
                );
                assert_eq!(
                    state.roster.sessions["session-a"].permission_mode,
                    Some(PermissionMode::Auto),
                    "pre-write fatal exhaustion cannot claim Plan or Bypass became effective"
                );
                assert!(
                    !state.roster.sessions["session-a"]
                        .guard_attempts
                        .contains_key(&attempt.token)
                );
                drop(state);
                let disk = Roster::load();
                assert_eq!(
                    disk.sessions["session-a"].permission_mode,
                    Some(PermissionMode::Auto)
                );
                assert!(disk.sessions["session-a"].guard_attempts.is_empty());
            });
    });
}

#[test]
fn a_coalesced_initial_retry_clears_only_its_prearm_token() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let attempt = crate::rc::harness::TurnGuardAttempt {
                    token: "coalesced-retry".into(),
                    expected_mode: PermissionMode::Plan,
                };
                let (tx, _rx) = mpsc::channel(1);
                let mut live = rpc_test_live("session-a", 1, tx, PermissionMode::Auto);
                live.pending_mode = Some(PermissionMode::Plan);
                live.inflight_turn_guard = Some(attempt.token.clone());
                let mut entry = rpc_test_roster_entry("session-a", PermissionMode::Auto);
                entry.guard_attempts.insert(
                    attempt.token.clone(),
                    roster::GuardAttempt {
                        expected_mode: PermissionMode::Plan,
                        observed: false,
                    },
                );
                entry.guard_attempts.insert(
                    "unrelated".into(),
                    roster::GuardAttempt {
                        expected_mode: PermissionMode::Bypass,
                        observed: true,
                    },
                );
                let mut roster = Roster::default();
                roster.sessions.insert("session-a".into(), entry);
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);

                let (completion, response) = project_turn_start_outcome(
                    TurnStartOutcome::ConcurrentNotAccepted {
                        message: "creation prompt already owns this text".into(),
                    },
                    Some(attempt.clone()),
                );
                assert!(response.is_err());
                daemon
                    .lock()
                    .await
                    .complete_session_rpc("session-a", 1, &completion)
                    .unwrap();

                let state = daemon.lock().await;
                assert_eq!(
                    state.sessions["session-a"].info.permission_mode,
                    Some(PermissionMode::Auto),
                    "coalescing does not claim the later mode is live"
                );
                assert_eq!(
                    state.sessions["session-a"].pending_mode,
                    Some(PermissionMode::Plan),
                    "the later mode remains queued for the following turn"
                );
                assert!(state.sessions["session-a"].inflight_turn_guard.is_none());
                assert_eq!(
                    state.roster.sessions["session-a"]
                        .guard_attempts
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                    vec!["unrelated"]
                );
                drop(state);
                assert_eq!(
                    Roster::load().sessions["session-a"]
                        .guard_attempts
                        .keys()
                        .map(String::as_str)
                        .collect::<Vec<_>>(),
                    vec!["unrelated"]
                );
            });
    });
}

#[tokio::test]
async fn exact_turn_completion_requires_the_matching_prearm_token() {
    use crate::protocol::PermissionMode;

    let (tx, _rx) = mpsc::channel(1);
    let live = rpc_test_live("session-a", 1, tx, PermissionMode::Auto);
    let mut roster = Roster::default();
    roster.sessions.insert(
        "session-a".into(),
        rpc_test_roster_entry("session-a", PermissionMode::Auto),
    );
    let daemon = rpc_test_daemon([("session-a".into(), live)].into_iter().collect(), roster);
    let attempt = crate::rc::harness::TurnGuardAttempt {
        token: "missing".into(),
        expected_mode: PermissionMode::Plan,
    };
    let completion = SessionRpcCompletion::Turn {
        guard_attempt: Some(attempt.clone()),
        accepted_mode: Some(PermissionMode::Plan),
        confirmation: Some(TurnStartConfirmation::Exact),
        fail_closed: false,
        retire_generation: false,
    };
    let error = daemon
        .lock()
        .await
        .complete_session_rpc("session-a", 1, &completion)
        .expect_err("a missing CAS token cannot be treated as already cleared");
    assert!(error.to_string().contains("lost its pre-dispatch"));
    assert_eq!(
        daemon.lock().await.sessions["session-a"]
            .info
            .permission_mode,
        Some(PermissionMode::Auto),
        "validation happens before visible state mutation"
    );

    daemon
        .lock()
        .await
        .roster
        .sessions
        .get_mut("session-a")
        .unwrap()
        .guard_attempts
        .insert(
            attempt.token.clone(),
            roster::GuardAttempt {
                expected_mode: PermissionMode::Bypass,
                observed: false,
            },
        );
    let error = daemon
        .lock()
        .await
        .complete_session_rpc("session-a", 1, &completion)
        .expect_err("the same token cannot change its expected mode");
    assert!(error.to_string().contains("no longer matches"));
}
