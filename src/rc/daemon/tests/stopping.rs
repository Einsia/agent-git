use super::*;

#[test]
fn hard_stop_token_allocation_skips_an_occupied_uuid_candidate() {
    use crate::protocol::PermissionMode;

    let occupied = format!("{}collision", roster::SHUTDOWN_GUARD_PREFIX);
    let attempts = std::collections::BTreeMap::from([(
        occupied,
        roster::GuardAttempt {
            expected_mode: PermissionMode::Plan,
            observed: false,
        },
    )]);
    let mut candidates =
        std::collections::VecDeque::from(["collision".to_string(), "fresh".to_string()]);
    let token = fresh_shutdown_guard_token_with(Some(&attempts), || {
        candidates.pop_front().expect("enough candidates")
    });
    assert_eq!(token, format!("{}fresh", roster::SHUTDOWN_GUARD_PREFIX));
}

/// Queued RPCs enter through `prepare_session_rpc`, outside the ordinary
/// dispatcher. They must still use the one centralized method-role table;
/// a viewer is rejected before a command or serial gate is consumed.
#[tokio::test]
async fn prepared_session_rpc_uses_the_central_role_gate() {
    let (tx, mut rx) = mpsc::channel(1);
    let daemon = rpc_test_daemon(
        [(
            "session-a".into(),
            rpc_test_live("session-a", 1, tx, crate::protocol::PermissionMode::Default),
        )]
        .into_iter()
        .collect(),
        Roster::default(),
    );
    let mut frame = Frame::request(
        method::TURN_INTERRUPT,
        TurnInterrupt {
            session_id: "session-a".into(),
            by: Some("viewer".into()),
        },
    );
    frame.caller = Some(claim("viewer", "ws-a"));

    let error = match daemon.lock().await.prepare_session_rpc(&frame) {
        Ok(_) => panic!("viewer entered the queued command path"),
        Err(error) => error,
    };
    assert_eq!(error.code, ErrorCode::Forbidden as i32);
    assert!(
        matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "authorization must happen before queueing"
    );
    let state = daemon.lock().await;
    assert!(
        state.sessions["session-a"].rpc_gate.try_lock().is_ok(),
        "a rejected request must not consume the session gate"
    );
}

/// Stop is a cancellation boundary for an instruction the supervisor has
/// not accepted. The worker must win QUEUED -> ABANDONED, project the
/// generation-fenced danger rollback, and only then leave the JoinSet.
#[test]
fn stopping_with_a_queued_dangerous_rpc_rolls_back_its_durable_arm() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;
                let (tx, mut rx) = mpsc::channel(1);
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "session-a".into(),
                    rpc_test_roster_entry("session-a", PermissionMode::Default),
                );
                let daemon = rpc_test_daemon(
                    [(
                        "session-a".into(),
                        rpc_test_live("session-a", 1, tx, PermissionMode::Default),
                    )]
                    .into_iter()
                    .collect(),
                    roster,
                );

                let prepared = daemon
                    .lock()
                    .await
                    .prepare_session_rpc(&rpc_mode_frame(PermissionMode::Bypass))
                    .expect("owner can prepare a dangerous mode change");
                {
                    let state = daemon.lock().await;
                    let live = state.sessions.get("session-a").unwrap();
                    assert!(live.info.dangerous, "prepare durably arms first");
                    assert!(live.rpc_guard_sensitive);
                    assert!(state.roster.sessions["session-a"].ever_dangerous);
                }

                let (stop_tx, mut stop) = tokio::sync::watch::channel(false);
                let worker_daemon = daemon.clone();
                let worker =
                    tokio::spawn(async move { prepared.execute(worker_daemon, &mut stop).await });
                let queued = tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
                    .await
                    .expect("command enters the session queue")
                    .expect("session queue stays open");
                stop_tx.send_replace(true);
                let result = tokio::time::timeout(std::time::Duration::from_millis(200), worker)
                    .await
                    .expect("queued worker cancels promptly")
                    .expect("worker joins");
                assert!(result.pending.is_none());
                assert!(result.response.is_err());

                match queued {
                    Command::SetPermissionMode { reply, .. } => assert!(
                        !reply.accept(),
                        "a command withdrawn for shutdown must never execute later"
                    ),
                    _ => panic!("wrong queued command"),
                }
                {
                    let state = daemon.lock().await;
                    let live = state.sessions.get("session-a").unwrap();
                    assert!(!live.info.dangerous);
                    assert!(!live.rpc_guard_sensitive);
                    assert!(!state.roster.sessions["session-a"].ever_dangerous);
                    assert!(
                        !roster::has_shutdown_guard(
                            &state.roster.sessions["session-a"].guard_attempts
                        ),
                        "a command proven NeverRan never arms its reserved recovery token"
                    );
                }
                assert!(
                    !Roster::load().sessions["session-a"].ever_dangerous,
                    "the rollback must survive daemon restart"
                );
            });
    });
}

/// A supervisor closes a viewer RPC receipt only after it has enqueued the
/// resulting machine facts. Even when the hard deadline wins against that
/// completed-but-unjoined worker (while another worker remains stuck), stop
/// must project the ready worker's prefix before sessions are torn down.
/// This pins both tails that matter to the guard: an explicit mode refusal's
/// disarm note and Codex's NextTurn permission event.
#[test]
fn hard_deadline_drains_guard_projections_from_a_ready_worker() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::{PermissionApply, PermissionMode};

                let (command_tx, _command_rx) = mpsc::channel(1);
                let mut live = rpc_test_live("session-a", 1, command_tx, PermissionMode::Auto);
                live.danger_arm = 7;
                live.info.dangerous = true;
                // `complete_session_rpc` has already recorded the result;
                // the supervisor's frame is the remaining hub-visible fact.
                live.pending_mode = Some(PermissionMode::Plan);
                let mut roster = Roster::default();
                let mut entry = rpc_test_roster_entry("session-a", PermissionMode::Auto);
                entry.ever_dangerous = true;
                roster.sessions.insert("session-a".into(), entry);
                let (stuck_tx, _stuck_rx) = mpsc::channel(1);
                let mut stuck = rpc_test_live("stuck", 2, stuck_tx, PermissionMode::Auto);
                stuck.rpc_guard_sensitive = true;
                let stuck_gate = stuck.rpc_gate.clone().try_lock_owned().unwrap();
                roster.sessions.insert(
                    "stuck".into(),
                    rpc_test_roster_entry("stuck", PermissionMode::Auto),
                );
                roster.save().unwrap();
                let daemon = rpc_test_daemon(
                    [("session-a".into(), live), ("stuck".into(), stuck)]
                        .into_iter()
                        .collect(),
                    roster,
                );

                let (frames_tx, mut frames_rx) = mpsc::channel(4);
                let (notes_tx, mut notes_rx) = mpsc::channel(4);
                let mut permission = Frame::notification(
                    method::SESSION_PERMISSION_MODE,
                    crate::protocol::SessionPermissionMode {
                        session_id: "session-a".into(),
                        mode: PermissionMode::Plan,
                        applied: PermissionApply::NextTurn,
                        by: Some("owner".into()),
                    },
                );
                permission.stream = Some("session-a".into());

                // Model the production ordering: both sends complete before
                // the RPC worker itself becomes joinable. Do not poll either
                // receiver yet, so JoinSet wins exactly the old bad race.
                let mut workers = tokio::task::JoinSet::new();
                let producer_frames = frames_tx.clone();
                let producer_notes = notes_tx.clone();
                let producer = workers.spawn(async move {
                    producer_frames.send(permission).await.unwrap();
                    producer_notes
                        .send(SessionNote::DangerDisarmed {
                            session_id: "session-a".into(),
                            generation: 1,
                            arm: 7,
                        })
                        .await
                        .unwrap();
                });
                let stuck_worker = workers.spawn(async move {
                    let _gate = stuck_gate;
                    std::future::pending::<()>().await;
                });
                while !producer.is_finished() {
                    tokio::task::yield_now().await;
                }
                assert!(
                    !stuck_worker.is_finished(),
                    "one guard-sensitive worker still reaches the hard deadline"
                );
                assert!(
                    !workers.is_empty(),
                    "the completed producer is still unjoined when the deadline wins"
                );

                finish_session_rpcs_at_deadline(&daemon, &mut workers).await;
                assert!(workers.is_empty(), "the deadline establishes the barrier");

                let mut tail = ShutdownProjectionTail::capture(&frames_rx, &notes_rx);
                assert_eq!(
                    tail,
                    ShutdownProjectionTail {
                        frames: 1,
                        notes: 1
                    }
                );

                // Live sessions may emit after the barrier. The captured
                // prefix is fixed: these later items must not make shutdown
                // wait forever or be mistaken for RPC-owned projections.
                let mut later = Frame::notification(method::ITEM_DELTA, serde_json::json!({}));
                later.stream = Some("session-a".into());
                frames_tx.send(later).await.unwrap();
                notes_tx
                    .send(SessionNote::Ended {
                        session_id: "session-a".into(),
                        generation: 99,
                    })
                    .await
                    .unwrap();

                let (outbound, mut outbound_rx) = crate::rc::outbound::channel();
                let (tail_tx, _tail_rx) = mpsc::unbounded_channel();
                let ordered = OrderedTail::new(tail_tx, Default::default());

                while !tail.complete() {
                    if tail.frames > 0 {
                        let frame = frames_rx.recv().await.unwrap();
                        tail.took_frame();
                        let frame = daemon
                            .lock()
                            .await
                            .project_session_frame(frame)
                            .expect("permission fact survives the projection gate");
                        assert_eq!(
                            send_live_frame(&outbound, &ordered, frame),
                            crate::rc::outbound::Sent::Queued
                        );
                    }
                    if tail.notes > 0 {
                        let note = notes_rx.recv().await.unwrap();
                        tail.took_note();
                        daemon.lock().await.on_session_note(note);
                    }
                }

                let sent = outbound_rx.next_write().await.unwrap();
                assert_eq!(sent.frame().method(), method::SESSION_PERMISSION_MODE);
                assert_eq!(sent.frame().seq, Some(1));
                sent.commit();
                assert_eq!(
                    frames_rx.len(),
                    1,
                    "post-barrier frame stays outside the tail"
                );
                assert_eq!(
                    notes_rx.len(),
                    1,
                    "post-barrier note stays outside the tail"
                );

                let state = daemon.lock().await;
                let live = &state.sessions["session-a"];
                assert_eq!(live.info.permission_mode, Some(PermissionMode::Auto));
                assert_eq!(live.pending_mode, Some(PermissionMode::Plan));
                assert!(!live.info.dangerous, "the explicit refusal is projected");
                assert_eq!(state.journal.last_seq("session-a"), 1);
                assert!(!state.roster.sessions["session-a"].ever_dangerous);
                assert_eq!(
                    state.sessions["stuck"].info.permission_mode,
                    Some(PermissionMode::Plan),
                    "the genuinely in-flight guard still fails closed before abort"
                );
                drop(state);
                assert!(!Roster::load().sessions["session-a"].ever_dangerous);
            });
    });
}

#[test]
fn a_primary_save_failure_restarts_from_the_durable_plan_fallback() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let (command_tx, _command_rx) = mpsc::channel(1);
                let mut live = rpc_test_live("guard", 1, command_tx, PermissionMode::Auto);
                live.rpc_guard_sensitive = true;
                let gate = live.rpc_gate.clone().try_lock_owned().unwrap();
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "guard".into(),
                    rpc_test_roster_entry("guard", PermissionMode::Auto),
                );
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("guard".into(), live)].into_iter().collect(), roster);

                roster::fail_next_saves(1, 0);
                daemon
                    .lock()
                    .await
                    .fail_closed_inflight_session_rpcs()
                    .expect("the complete Plan snapshot falls back durably");
                let rc_dir = crate::rc::rc_dir().unwrap();
                assert!(rc_dir.join("sessions.fail-closed.json").exists());
                let fallback: Roster = serde_json::from_slice(
                    &std::fs::read(rc_dir.join("sessions.fail-closed.json")).unwrap(),
                )
                .unwrap();
                let fallback_entry = &fallback.sessions["guard"];
                assert!(roster::has_shutdown_guard(&fallback_entry.guard_attempts));
                assert_eq!(
                    fallback_entry.permission_mode,
                    Some(PermissionMode::Plan),
                    "guard-unaware readers still see the direct Plan floor"
                );
                assert_eq!(
                    fallback_entry.restart_permission_mode(),
                    Some(PermissionMode::Plan)
                );
                let old_primary: Roster =
                    serde_json::from_slice(&std::fs::read(rc_dir.join("sessions.json")).unwrap())
                        .unwrap();
                assert_eq!(
                    old_primary.sessions["guard"].permission_mode,
                    Some(PermissionMode::Auto),
                    "the injected failure really leaves a looser primary"
                );

                let restarted = Roster::try_load().expect("restart promotes the fallback");
                assert_eq!(
                    restarted.sessions["guard"].permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert!(roster::has_shutdown_guard(
                    &restarted.sessions["guard"].guard_attempts
                ));
                assert!(!rc_dir.join("sessions.fail-closed.json").exists());
                drop(gate);
            });
    });
}

#[test]
fn hard_stop_floor_survives_both_active_fallback_failure_points() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let (tx, _rx) = mpsc::channel(1);
                let mut live = rpc_test_live("guard", 1, tx, PermissionMode::Bypass);
                live.info.dangerous = true;
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "guard".into(),
                    rpc_test_roster_entry("guard", PermissionMode::Bypass),
                );
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("guard".into(), live)].into_iter().collect(), roster);
                let mut base = hard_stop_guard("guard", 1, "base");
                base.dangerous = true;

                // Establish an authoritative fallback snapshot.
                roster::fail_next_saves(1, 0);
                daemon
                    .lock()
                    .await
                    .persist_fail_closed_session_rpcs(std::slice::from_ref(&base))
                    .expect("primary failure writes the complete fallback");

                let fallback_write = hard_stop_guard("guard", 1, "fallback-write");
                roster::fail_next_saves(0, 1);
                assert!(
                    daemon
                        .lock()
                        .await
                        .persist_fail_closed_session_rpcs(std::slice::from_ref(&fallback_write))
                        .is_err(),
                    "an active fallback write failure keeps the batch pending"
                );
                {
                    let state = daemon.lock().await;
                    let entry = &state.roster.sessions["guard"];
                    assert!(entry.guard_attempts.contains_key(&base.token));
                    assert!(entry.guard_attempts.contains_key(&fallback_write.token));
                    assert_eq!(entry.permission_mode, Some(PermissionMode::Plan));
                    assert!(entry.ever_dangerous);
                }
                daemon
                    .lock()
                    .await
                    .persist_fail_closed_session_rpcs(std::slice::from_ref(&fallback_write))
                    .expect("the same token retries after fallback writability returns");

                // Reactivate fallback authority, then fail the primary
                // promotion after the new S snapshot is already durable.
                let reactivate = hard_stop_guard("guard", 1, "reactivate");
                roster::fail_next_saves(1, 0);
                daemon
                    .lock()
                    .await
                    .persist_fail_closed_session_rpcs(std::slice::from_ref(&reactivate))
                    .expect("fallback authority is re-established");
                let primary_promote = hard_stop_guard("guard", 1, "primary-promote");
                roster::fail_next_saves(1, 0);
                assert!(
                    daemon
                        .lock()
                        .await
                        .persist_fail_closed_session_rpcs(std::slice::from_ref(&primary_promote))
                        .is_err(),
                    "primary promotion failure must not roll memory back"
                );
                {
                    let state = daemon.lock().await;
                    let entry = &state.roster.sessions["guard"];
                    assert!(entry.guard_attempts.contains_key(&primary_promote.token));
                    assert!(entry.ever_dangerous);
                }
                let fallback: Roster = serde_json::from_slice(
                    &std::fs::read(
                        crate::rc::rc_dir()
                            .unwrap()
                            .join("sessions.fail-closed.json"),
                    )
                    .unwrap(),
                )
                .unwrap();
                assert!(
                    fallback.sessions["guard"]
                        .guard_attempts
                        .contains_key(&primary_promote.token)
                );

                daemon
                    .lock()
                    .await
                    .persist_fail_closed_session_rpcs(std::slice::from_ref(&primary_promote))
                    .expect("the identical frozen batch eventually promotes");

                let removal_reactivate = hard_stop_guard("guard", 1, "removal-reactivate");
                roster::fail_next_saves(1, 0);
                daemon
                    .lock()
                    .await
                    .persist_fail_closed_session_rpcs(std::slice::from_ref(&removal_reactivate))
                    .expect("fallback authority is re-established for removal testing");
                let removal = hard_stop_guard("guard", 1, "remove-failure");
                roster::fail_next_fallback_removals(1);
                assert!(
                    daemon
                        .lock()
                        .await
                        .persist_fail_closed_session_rpcs(std::slice::from_ref(&removal))
                        .is_err(),
                    "durable fallback removal failure keeps the batch pending"
                );
                {
                    let state = daemon.lock().await;
                    assert!(
                        state.roster.sessions["guard"]
                            .guard_attempts
                            .contains_key(&removal.token)
                    );
                }
                daemon
                    .lock()
                    .await
                    .persist_fail_closed_session_rpcs(std::slice::from_ref(&removal))
                    .expect("the same token retries after removal becomes durable");

                let disk = Roster::try_load().unwrap();
                assert!(
                    disk.sessions["guard"]
                        .guard_attempts
                        .contains_key(&primary_promote.token)
                );
                assert!(
                    disk.sessions["guard"]
                        .guard_attempts
                        .contains_key(&removal.token)
                );
                assert_eq!(
                    disk.sessions["guard"].restart_permission_mode(),
                    Some(PermissionMode::Plan)
                );
            });
    });
}

/// Saving an unchanged roster is not evidence that this session's Plan
/// restart policy exists. A missing row must retain the hard-stop barrier.
#[tokio::test]
async fn hard_stop_never_treats_a_missing_roster_row_as_durable() {
    use crate::protocol::PermissionMode;

    let (command_tx, _command_rx) = mpsc::channel(1);
    let live = rpc_test_live("guard", 1, command_tx, PermissionMode::Auto);
    let daemon = rpc_test_daemon(
        [("guard".into(), live)].into_iter().collect(),
        Roster::default(),
    );
    let error = daemon
        .lock()
        .await
        .persist_fail_closed_session_rpcs(&[hard_stop_guard("guard", 1, "missing-row")])
        .expect_err("saving no row is not termination authority");
    assert!(error.to_string().contains("no durable roster row"));
}

#[tokio::test]
async fn hard_stop_validates_the_whole_generation_batch_before_mutating() {
    use crate::protocol::PermissionMode;

    let (first_tx, _first_rx) = mpsc::channel(1);
    let (replaced_tx, _replaced_rx) = mpsc::channel(1);
    let mut roster = Roster::default();
    roster.sessions.insert(
        "first".into(),
        rpc_test_roster_entry("first", PermissionMode::Auto),
    );
    roster.sessions.insert(
        "replaced".into(),
        rpc_test_roster_entry("replaced", PermissionMode::Bypass),
    );
    let daemon = rpc_test_daemon(
        [
            (
                "first".into(),
                rpc_test_live("first", 1, first_tx, PermissionMode::Auto),
            ),
            (
                "replaced".into(),
                rpc_test_live("replaced", 2, replaced_tx, PermissionMode::Bypass),
            ),
        ]
        .into_iter()
        .collect(),
        roster,
    );
    let guards = [
        hard_stop_guard("first", 1, "first"),
        hard_stop_guard("replaced", 1, "stale-generation"),
    ];

    let mut state = daemon.lock().await;
    let error = state
        .persist_fail_closed_session_rpcs(&guards)
        .expect_err("a replacement generation cannot authorize cancellation");
    assert!(error.to_string().contains("generation 1 disappeared"));
    assert_eq!(
        state.sessions["first"].info.permission_mode,
        Some(PermissionMode::Auto),
        "an earlier valid row must not be partially tightened"
    );
    assert_eq!(
        state.roster.sessions["first"].permission_mode,
        Some(PermissionMode::Auto)
    );
    assert!(!roster::has_shutdown_guard(
        &state.roster.sessions["first"].guard_attempts
    ));
    assert!(!roster::has_shutdown_guard(
        &state.roster.sessions["replaced"].guard_attempts
    ));
}

/// Neither a failed primary write nor a failed fallback write authorizes
/// cancellation. The worker (and therefore its per-session gate) remains
/// alive until a later retry is durable.
#[test]
fn hard_stop_keeps_the_gate_until_a_persistence_retry_succeeds() {
    struct AbortWitness(std::sync::Arc<std::sync::atomic::AtomicBool>);
    impl Drop for AbortWitness {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let (command_tx, _command_rx) = mpsc::channel(1);
                let mut live = rpc_test_live("guard", 1, command_tx, PermissionMode::Auto);
                live.rpc_guard_sensitive = true;
                let gate = live.rpc_gate.clone().try_lock_owned().unwrap();
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "guard".into(),
                    rpc_test_roster_entry("guard", PermissionMode::Auto),
                );
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("guard".into(), live)].into_iter().collect(), roster);

                let aborted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let witness = AbortWitness(aborted.clone());
                let mut workers = tokio::task::JoinSet::new();
                workers.spawn(async move {
                    let _gate = gate;
                    let _witness = witness;
                    std::future::pending::<()>().await;
                });

                roster::fail_next_saves(1, 1);
                let closer_daemon = daemon.clone();
                let closer = tokio::spawn(async move {
                    finish_session_rpcs_at_deadline(&closer_daemon, &mut workers).await;
                    assert!(workers.is_empty());
                });
                while roster::pending_injected_saves() != (0, 0) {
                    tokio::task::yield_now().await;
                }

                assert!(
                    !aborted.load(std::sync::atomic::Ordering::SeqCst),
                    "both failed writes must retain the worker and its gate"
                );
                assert!(!closer.is_finished(), "hard stop must wait for durability");
                let frozen_token = {
                    let mut state = daemon.lock().await;
                    let live = &state.sessions["guard"];
                    assert_eq!(
                        live.info.permission_mode,
                        Some(PermissionMode::Plan),
                        "failed destinations must not roll back the live floor"
                    );
                    assert_eq!(live.pending_mode, None);
                    let entry = &state.roster.sessions["guard"];
                    assert_eq!(entry.permission_mode, Some(PermissionMode::Plan));
                    let mut shutdown = entry
                        .guard_attempts
                        .iter()
                        .filter(|(token, _)| token.starts_with(roster::SHUTDOWN_GUARD_PREFIX));
                    let (token, attempt) = shutdown.next().expect("shutdown token is armed");
                    assert!(shutdown.next().is_none(), "one deadline arms one token");
                    assert_eq!(attempt.expected_mode, PermissionMode::Plan);
                    assert!(!attempt.observed);
                    let token = token.clone();
                    // A TAKEN worker can settle and remove an already-ended
                    // generation during disk backoff. The armed token—not a
                    // stale Live handle—is the retry witness.
                    state.sessions.remove("guard");
                    token
                };
                let primary: Roster = serde_json::from_slice(
                    &std::fs::read(crate::rc::rc_dir().unwrap().join("sessions.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    primary.sessions["guard"].permission_mode,
                    Some(PermissionMode::Auto)
                );

                tokio::time::advance(FAIL_CLOSED_PERSIST_RETRY_MIN).await;
                closer.await.unwrap();
                assert!(aborted.load(std::sync::atomic::Ordering::SeqCst));
                assert_eq!(
                    Roster::try_load().unwrap().sessions["guard"].permission_mode,
                    Some(PermissionMode::Plan)
                );
                let restored = Roster::load();
                assert!(
                    restored.sessions["guard"]
                        .guard_attempts
                        .contains_key(&frozen_token)
                );
            });
    });
}

#[test]
fn hard_stop_freezes_token_and_danger_even_if_the_gate_later_releases() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let (tx, _rx) = mpsc::channel(1);
                let mut live = rpc_test_live("guard", 7, tx, PermissionMode::Bypass);
                live.rpc_guard_sensitive = true;
                live.info.dangerous = true;
                let gate = live.rpc_gate.clone().try_lock_owned().unwrap();
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "guard".into(),
                    rpc_test_roster_entry("guard", PermissionMode::Bypass),
                );
                roster.save().unwrap();
                let daemon =
                    rpc_test_daemon([("guard".into(), live)].into_iter().collect(), roster);

                let frozen = daemon.lock().await.inflight_guard_sensitive_session_rpcs();
                assert_eq!(frozen.len(), 1);
                assert_eq!(frozen[0].generation, 7);
                assert!(frozen[0].dangerous);
                assert!(frozen[0].token.starts_with(roster::SHUTDOWN_GUARD_PREFIX));

                drop(gate);
                {
                    let mut state = daemon.lock().await;
                    let live = state.sessions.get_mut("guard").unwrap();
                    live.rpc_guard_sensitive = false;
                    live.info.dangerous = false;
                    state
                        .persist_fail_closed_session_rpcs(&frozen)
                        .expect("the deadline snapshot, not rediscovery, is authoritative");
                    let entry = &state.roster.sessions["guard"];
                    assert!(entry.ever_dangerous, "danger is frozen at the deadline");
                    assert!(entry.guard_attempts.contains_key(&frozen[0].token));
                }
                let disk = Roster::load();
                assert!(disk.sessions["guard"].ever_dangerous);
                assert!(
                    disk.sessions["guard"]
                        .guard_attempts
                        .contains_key(&frozen[0].token)
                );
            });
    });
}

/// A TAKEN tightening cannot be canceled at stop: its worker must retain
/// the gate until the real receipt arrives and persist Plan before shutdown
/// is allowed to drain the live generation.
#[test]
fn stopping_waits_for_a_taken_tightening_to_reach_the_roster() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::{PermissionApply, PermissionMode};
                let (tx, mut rx) = mpsc::channel(1);
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "session-a".into(),
                    rpc_test_roster_entry("session-a", PermissionMode::Auto),
                );
                roster.save().unwrap();
                let daemon = rpc_test_daemon(
                    [(
                        "session-a".into(),
                        rpc_test_live("session-a", 1, tx, PermissionMode::Auto),
                    )]
                    .into_iter()
                    .collect(),
                    roster,
                );
                let frame = rpc_mode_frame(PermissionMode::Plan);
                let request_id = frame.id.clone().unwrap();
                let prepared = daemon
                    .lock()
                    .await
                    .prepare_session_rpc(&frame)
                    .expect("tightening is admitted");
                let (stop_tx, stop) = tokio::sync::watch::channel(false);
                let worker_daemon = daemon.clone();
                let (outbound, mut outbound_rx) = crate::rc::outbound::channel();
                let mut worker =
                    tokio::spawn(prepared.serve(worker_daemon, outbound, request_id.clone(), stop));
                let command =
                    tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv())
                        .await
                        .expect("command enters the session queue")
                        .expect("session queue stays open");
                let reply = match command {
                    Command::SetPermissionMode { reply, .. } => {
                        assert!(reply.accept(), "the supervisor takes the command");
                        reply
                    }
                    _ => panic!("wrong queued command"),
                };

                stop_tx.send_replace(true);
                assert!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(20),
                        outbound_rx.next_write(),
                    )
                    .await
                    .is_err(),
                    "stop cannot expose an early untyped result for a TAKEN mode change"
                );
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(20), &mut worker,)
                        .await
                        .is_err(),
                    "stop must not discard a TAKEN mode projection"
                );
                reply.finish(Ok(PermissionModeOutcome::Applied {
                    applied: PermissionApply::Immediate,
                }));
                let response = tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    outbound_rx.next_write(),
                )
                .await
                .expect("typed result produces the only response")
                .expect("response lane stays open");
                assert_eq!(response.frame().id.as_ref(), Some(&request_id));
                assert!(response.frame().error.is_none());
                response.commit();
                tokio::time::timeout(std::time::Duration::from_millis(200), worker)
                    .await
                    .expect("worker finishes after the real receipt")
                    .expect("worker joins");
                assert!(
                    outbound_rx.next_write().await.is_none(),
                    "one request id receives exactly one response"
                );
                {
                    let state = daemon.lock().await;
                    let live = state.sessions.get("session-a").unwrap();
                    assert_eq!(live.info.permission_mode, Some(PermissionMode::Plan));
                    assert!(!live.rpc_guard_sensitive);
                    assert_eq!(
                        state.roster.sessions["session-a"].permission_mode,
                        Some(PermissionMode::Plan)
                    );
                }
                assert_eq!(
                    Roster::load().sessions["session-a"].permission_mode,
                    Some(PermissionMode::Plan),
                    "the tightening must survive daemon restart"
                );
            });
    });
}

/// Codex consumes a queued NextTurn mode inside `start_turn`. The viewer
/// RPC therefore owns a guard mutation even though its method is Turn; an
/// unresolved hard stop must restart at Plan. A turn with no pending mode
/// remains ordinary and must not rewrite the user's durable guard.
#[test]
fn a_turn_that_can_consume_pending_mode_is_guard_sensitive() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;

                let (pending_tx, _pending_rx) = mpsc::channel(1);
                let mut pending = rpc_test_live("pending", 1, pending_tx, PermissionMode::Auto);
                pending.pending_mode = Some(PermissionMode::Plan);
                let (plain_tx, _plain_rx) = mpsc::channel(1);
                let plain = rpc_test_live("plain", 2, plain_tx, PermissionMode::Auto);
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "pending".into(),
                    rpc_test_roster_entry("pending", PermissionMode::Auto),
                );
                roster.sessions.insert(
                    "plain".into(),
                    rpc_test_roster_entry("plain", PermissionMode::Auto),
                );
                roster.save().unwrap();
                let daemon = rpc_test_daemon(
                    [("pending".into(), pending), ("plain".into(), plain)]
                        .into_iter()
                        .collect(),
                    roster,
                );
                let turn = |session_id: &str| {
                    let mut frame = Frame::request(
                        method::TURN_START,
                        TurnStart {
                            session_id: session_id.into(),
                            message: "hello".into(),
                            by: Some("owner".into()),
                            client_msg_id: Some(format!("message-{session_id}")),
                        },
                    );
                    frame.caller = Some(claim("owner", "ws-a"));
                    frame
                };

                let pending_request = daemon
                    .lock()
                    .await
                    .prepare_session_rpc(&turn("pending"))
                    .expect("pending-mode turn is admitted");
                let plain_request = daemon
                    .lock()
                    .await
                    .prepare_session_rpc(&turn("plain"))
                    .expect("ordinary turn is admitted");
                {
                    let state = daemon.lock().await;
                    assert!(state.sessions["pending"].rpc_guard_sensitive);
                    assert!(!state.sessions["plain"].rpc_guard_sensitive);
                }

                daemon
                    .lock()
                    .await
                    .fail_closed_inflight_session_rpcs()
                    .unwrap();
                let state = daemon.lock().await;
                assert_eq!(
                    state.sessions["pending"].info.permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert_eq!(state.sessions["pending"].pending_mode, None);
                assert_eq!(
                    state.sessions["plain"].info.permission_mode,
                    Some(PermissionMode::Auto),
                    "a plain turn must not cause gratuitous fail-closed tightening"
                );
                drop(state);
                let restored = Roster::load();
                assert_eq!(
                    restored.sessions["pending"].permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert_eq!(
                    restored.sessions["plain"].permission_mode,
                    Some(PermissionMode::Auto)
                );
                drop((pending_request, plain_request));
            });
    });
}

/// The hard stop fallback is deliberately narrow: only an unresolved RPC
/// that can mutate the guard is forced to Plan. A stuck ordinary interrupt
/// still owns a gate, but must not rewrite the user's durable mode.
#[test]
fn hard_stop_fail_closed_is_limited_to_guard_mutations() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                use crate::protocol::PermissionMode;
                let (guard_tx, _guard_rx) = mpsc::channel(1);
                let (plain_tx, _plain_rx) = mpsc::channel(1);
                let mut guard_live = rpc_test_live("guard", 1, guard_tx, PermissionMode::Auto);
                guard_live.rpc_guard_sensitive = true;
                let plain_live = rpc_test_live("plain", 2, plain_tx, PermissionMode::Auto);
                let guard_gate = guard_live.rpc_gate.clone().try_lock_owned().unwrap();
                let plain_gate = plain_live.rpc_gate.clone().try_lock_owned().unwrap();
                let mut roster = Roster::default();
                roster.sessions.insert(
                    "guard".into(),
                    rpc_test_roster_entry("guard", PermissionMode::Auto),
                );
                roster.sessions.insert(
                    "plain".into(),
                    rpc_test_roster_entry("plain", PermissionMode::Auto),
                );
                roster.save().unwrap();
                let daemon = rpc_test_daemon(
                    [("guard".into(), guard_live), ("plain".into(), plain_live)]
                        .into_iter()
                        .collect(),
                    roster,
                );

                daemon
                    .lock()
                    .await
                    .fail_closed_inflight_session_rpcs()
                    .unwrap();
                let state = daemon.lock().await;
                assert_eq!(
                    state.sessions["guard"].info.permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert_eq!(
                    state.sessions["plain"].info.permission_mode,
                    Some(PermissionMode::Auto)
                );
                drop(state);
                let restored = Roster::load();
                assert_eq!(
                    restored.sessions["guard"].permission_mode,
                    Some(PermissionMode::Plan)
                );
                assert_eq!(
                    restored.sessions["plain"].permission_mode,
                    Some(PermissionMode::Auto)
                );
                drop((guard_gate, plain_gate));
            });
    });
}

/// When the instruction that timed out **has not been taken yet**, the answer must be "nothing
/// happened, safe to retry" — and it never executes afterwards (the withdrawal wins the CAS).
#[tokio::test(start_paused = true)]
async fn an_instruction_nobody_picked_up_is_withdrawn_not_left_pending() {
    let (t, mut r) = crate::rc::ticket::ticket::<()>();
    let err = reply_within(&mut r)
        .await
        .expect_err("the reply must time out");
    assert_eq!(err.code, ErrorCode::SessionBusy as i32);
    let hint = err
        .data
        .as_ref()
        .and_then(|d| d.get("hint"))
        .and_then(|h| h.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        hint.contains("nothing happened"),
        "the caller must know this had no side effect: {hint:?}"
    );
    assert!(!t.accept(), "a withdrawn instruction can never execute");
}

/// An instruction already taken cannot be withdrawn. The answer here must **not** be "safe to
/// retry" — the side effect is already on its way, and making the caller do it twice is exactly
/// what must be avoided.
#[tokio::test(start_paused = true)]
async fn an_instruction_already_taken_is_awaited_not_declared_failed() {
    let (t, mut r) = crate::rc::ticket::ticket::<u8>();
    assert!(t.accept(), "the executing side takes it first");
    // The first leg times out and it waits one more leg; the result arrives during that one.
    tokio::spawn(async move {
        tokio::time::sleep(SESSION_REPLY_TIMEOUT + std::time::Duration::from_secs(1)).await;
        t.finish(Ok(9));
    });
    assert_eq!(reply_within(&mut r).await.expect("the result arrives"), 9);
}

/// A taken instruction that then goes silent is reported as it is — "started, outcome unknown" —
/// never as `SessionBusy`.
#[tokio::test(start_paused = true)]
async fn a_taken_instruction_that_never_answers_is_not_reported_as_retryable() {
    let (t, mut r) = crate::rc::ticket::ticket::<u8>();
    assert!(t.accept());
    std::mem::forget(t); // neither answers nor closes the channel
    let err = reply_within(&mut r)
        .await
        .expect_err("the reply must time out");
    assert_ne!(
        err.code,
        ErrorCode::SessionBusy as i32,
        "this one must not be reported as safe to retry"
    );
}

/// **No stamp, no service.**
///
/// If `session_owned_by` took `caller: Option<&_>` and let `None` **skip** the ownership test,
/// missing credentials would read as a wildcard — the exact inverse of `require_owner_for_danger`,
/// where `None` takes the strictest reading.
#[test]
fn a_frame_without_a_caller_claim_is_refused_rather_than_treated_as_a_wildcard() {
    let f = frame_with(None, serde_json::json!({ "workspace_id": "ws-a" }));
    let err = caller_scope(&f).expect_err("a frame with no stamp must be refused");
    assert_eq!(err.code, ErrorCode::Unauthenticated as i32);
}

/// The `workspace_id` in params comes from the browser and must match the one the hub stamped.
///
/// Every arm that reads it (`session.list` / `watch` / `fs.readFile` / `terminal.open` /
/// `project.bind` / `session.start` / `resume` / `unwatch`) compares the two; an arm that reads
/// it without comparing lets a member of A point that verb at B on the same machine.
#[test]
fn params_cannot_name_a_workspace_the_caller_was_not_stamped_for() {
    let f = frame_with(
        Some(claim("owner", "ws-a")),
        serde_json::json!({ "workspace_id": "ws-b" }),
    );
    let err = caller_scope(&f).expect_err("a cross-workspace claim must be refused");
    // The same answer as "this machine has no such workspace": confirming that it exists is
    // itself a leak.
    assert_eq!(err.code, ErrorCode::WorkspaceNotFound as i32);

    // Its own workspace is allowed as usual; so is a frame that names no workspace.
    assert!(
        caller_scope(&frame_with(
            Some(claim("operator", "ws-a")),
            serde_json::json!({ "workspace_id": "ws-a" })
        ))
        .is_ok()
    );
    assert!(
        caller_scope(&frame_with(
            Some(claim("operator", "ws-a")),
            serde_json::json!({ "session_id": "agit-x" })
        ))
        .is_ok()
    );
}

/// **The role is decided on the machine too, not only on the hub.**
///
/// `caller_scope` decides tenancy only. Without this table a request carrying a legitimate
/// **viewer** claim reaches `fs.readDirectory` (which lists the home directory, unrelated to any
/// workspace), `project.bind` (which adds a directory to the allowlist) and `terminal.open`
/// (which opens a real PTY) — three verbs otherwise locked on the hub alone, and the hub is a
/// relay, not the authority.
#[test]
fn a_viewer_claim_cannot_reach_the_verbs_that_act_on_the_machine_itself() {
    let viewer = claim("viewer", "ws-a");
    let operator = claim("operator", "ws-a");
    let owner = claim("owner", "ws-a");

    // Acting on the machine itself: owner only.
    for m in [
        method::FS_READ_DIRECTORY,
        method::PROJECT_BIND,
        method::TERMINAL_OPEN,
        method::TERMINAL_INPUT,
    ] {
        assert!(
            require_role(&viewer, m).is_err(),
            "a viewer must not reach {m}"
        );
        assert!(
            require_role(&operator, m).is_err(),
            "an operator must not reach {m}"
        );
        assert!(require_role(&owner, m).is_ok(), "{m}");
    }

    // Driving a session: operator is the floor.
    for m in [
        method::TURN_START,
        method::APPROVAL_DECIDE,
        method::SESSION_START,
    ] {
        assert!(
            require_role(&viewer, m).is_err(),
            "a viewer must not drive {m}"
        );
        assert!(require_role(&operator, m).is_ok(), "{m}");
    }

    // Pure reads: viewer is enough.
    for m in [
        method::SESSION_LIST,
        method::SESSION_SUBSCRIBE,
        method::WORKSPACE_LIST,
    ] {
        assert!(require_role(&viewer, m).is_ok(), "{m}");
    }
}

/// An unregistered verb takes the strictest role — **forgetting to register a new verb costs at
/// worst "nobody but the owner can use it", never "anybody can use it"**.
#[test]
fn a_verb_nobody_remembered_to_register_defaults_to_owner_only() {
    assert!(require_role(&claim("operator", "ws-a"), "some.futureVerb").is_err());
    assert!(require_role(&claim("owner", "ws-a"), "some.futureVerb").is_ok());
}

/// **Braking needs no permission.**
///
/// Interrupting, denying an approval, tightening the guard — all three **subtract** from what
/// this session can do. Putting them behind the danger gate costs this: a bypass session that
/// has run away can be stopped by the owner alone, who may be asleep; an approval the operator
/// wants to deny hangs on forever with the session stuck at `awaiting_approval`; a session
/// already at bypass has to ask even to "switch back to plan" — that is, "you must ask
/// permission to become safe".
#[test]
fn braking_a_dangerous_session_does_not_need_the_owner_but_driving_it_does() {
    let operator = claim("operator", "ws-a");
    // Drive: feeding a message into a dangerous session / allowing an approval — owner only.
    assert!(
        require_owner_to_drive(Some(&operator), true).is_err(),
        "an operator must not drive an ever-dangerous session"
    );
    // Driving under an **ordinary mode** needs no owner — `accept_edits` / `auto` are the
    // working modes an owner switches to precisely so collaborators can keep moving. Conflating
    // "switching into this mode needs the owner" with "working inside this mode needs the owner"
    // turns the second half into a rule that shuts off the common use of the feature.
    assert!(
        require_owner_to_drive(Some(&operator), false).is_ok(),
        "an operator can drive a session that has never been dangerous"
    );
    // Loosening to `accept_edits` / `auto` is owner-only as well: under those modes claude-code
    // stops sending `can_use_tool` at all, which switches the fail-closed classifier off
    // entirely — while the frame itself is perfectly legal. The test must be `loosens_guard`,
    // not `is_dangerous`.
    let owner = claim("owner", "ws-a");
    for loosening in [
        crate::protocol::PermissionMode::AcceptEdits,
        crate::protocol::PermissionMode::Auto,
        crate::protocol::PermissionMode::Bypass,
    ] {
        assert!(
            require_owner_to_loosen(
                Some(&operator),
                loosening,
                Some(crate::protocol::PermissionMode::Default)
            )
            .is_err(),
            "{loosening:?} loosens; only the owner can switch to it"
        );
        assert!(
            require_owner_to_loosen(
                Some(&owner),
                loosening,
                Some(crate::protocol::PermissionMode::Default)
            )
            .is_ok()
        );
    }
    // Tightening never needs permission — "you must ask permission to become safe" makes no sense.
    for tightening in [
        crate::protocol::PermissionMode::Plan,
        crate::protocol::PermissionMode::Default,
    ] {
        assert!(
            require_owner_to_loosen(
                Some(&operator),
                tightening,
                Some(crate::protocol::PermissionMode::Bypass)
            )
            .is_ok(),
            "{tightening:?} tightens; an operator may do it at any time"
        );
    }
    // The Brake path never calls `require_owner_for_danger`, so what this pins is the
    // classification itself: a tightening mode is not `loosens_guard`, and interrupt and Deny
    // both go through Brake.
    for tightening in [
        crate::protocol::PermissionMode::Plan,
        crate::protocol::PermissionMode::Default,
    ] {
        assert!(
            !tightening.loosens_guard(),
            "{tightening:?} tightens and must not count as loosening"
        );
    }
    for loosening in [
        crate::protocol::PermissionMode::AcceptEdits,
        crate::protocol::PermissionMode::Auto,
        crate::protocol::PermissionMode::Bypass,
    ] {
        assert!(
            loosening.loosens_guard(),
            "{loosening:?} makes the harness ask less or not at all, so it loosens"
        );
    }
}

/// A read-only watch stream id must carry the workspace.
///
/// One directory can be bound once by each of two workspaces, so A and B watch the same local
/// session. Colliding on one key costs three things: a single unwatch from A cuts off B's tail;
/// the hub's `remember_workspace` is last-write-wins, so events fan out to the wrong workspace;
/// and once unwatch checks ownership, the workspace that bound second can never subtract its own
/// watch, and the poll stays on the machine.
#[test]
fn two_workspaces_watching_the_same_local_session_do_not_share_one_stream() {
    assert_ne!(
        watch_stream_id("ws-a", "thread-1"),
        watch_stream_id("ws-b", "thread-1")
    );
    assert!(watch_stream_id("ws-a", "thread-1").starts_with("agit-watch-"));
}

/// When a session is adopted from the terminal, only the hub can supply the lineage — the
/// machine side knows nothing about that session. It has to make the whole round trip: packed
/// into `AGIT_SESSION`, and unpacked again at settlement.
#[test]
fn a_lineage_handed_over_by_the_hub_round_trips_through_agit_session() {
    const ID: &str = "00000000-0000-0000-0000-000000000001";
    let l = lineage_from_params(true, Some("acme/api"), Some(ID), Some("s/2f1a"))
        .unwrap()
        .expect("all three parts given yields a lineage");
    assert_eq!(l.to_string(), "acme/api@s/2f1a");
    assert_eq!(l.agent_id(), ID);
    // The three parts must come back exactly as they went in — one byte off in `AGIT_SESSION`
    // and the child process settles into a different agent repo.
    let back = crate::rc::lineage::AgitSession::parse(&l.to_string(), ID).unwrap();
    assert_eq!(back.slug(), "acme/api");
    assert_eq!(back.agent_id(), ID);
    assert_eq!(back.branch(), "s/2f1a");
}

/// After negotiation the lineage must be the complete triple; without negotiation an old hub's
/// slug-only lineage is display data only.
#[test]
fn only_a_fully_negotiated_identity_becomes_settlement_lineage() {
    const ID: &str = "00000000-0000-0000-0000-000000000001";
    assert_eq!(
        lineage_from_params(false, Some("acme/api"), None, Some("s/2f1a")).unwrap(),
        None,
        "old hub/new daemon must run without settlement"
    );
    assert_eq!(lineage_from_params(true, None, None, None).unwrap(), None);
    for (a, id, b) in [
        (Some("acme/api"), None, Some("s/2f1a")),
        (Some("acme/api"), Some(ID), None),
        (None, Some(ID), Some("s/2f1a")),
        (Some("acme/api"), Some(""), Some("s/2f1a")),
    ] {
        let err = lineage_from_params(true, a, id, b)
            .expect_err(&format!("partial lineage {a:?} {id:?} {b:?}"));
        assert_eq!(err.code, ErrorCode::MalformedFrame as i32);
    }
}

#[test]
fn a_legacy_roster_lineage_never_adopts_the_current_slug_identity() {
    let legacy = roster::Entry {
        runtime: "codex".into(),
        thread_id: "thread-1".into(),
        cwd: "/tmp/project".into(),
        workspace_id: "ws-1".into(),
        project_id: Some("project-1".into()),
        agit_session: Some("acme/api@s/1".into()),
        expected_agent_id: None,
        permission_mode: None,
        guard_attempts: Default::default(),
        prior_threads: vec![],
        ever_dangerous: false,
    };
    assert_eq!(
        resume_lineage(
            true,
            &legacy,
            Some("acme/api"),
            Some("00000000-0000-0000-0000-000000000002"),
            Some("s/1")
        )
        .unwrap(),
        None,
        "the currently negotiated hub cannot fill in a missing historical ID"
    );

    let fenced = roster::Entry {
        expected_agent_id: Some("00000000-0000-0000-0000-000000000001".into()),
        ..legacy
    };
    assert_eq!(
        roster_lineage(false, &fenced),
        None,
        "a complete row still needs the current connection ACK"
    );
    assert_eq!(
        roster_lineage(true, &fenced)
            .expect("complete negotiated row")
            .agent_id(),
        "00000000-0000-0000-0000-000000000001"
    );

    let unclaimed = roster::Entry {
        agit_session: None,
        expected_agent_id: None,
        ..fenced
    };
    assert_eq!(
        resume_lineage(
            true,
            &unclaimed,
            Some("acme/api"),
            Some("00000000-0000-0000-0000-000000000002"),
            Some("s/2")
        )
        .unwrap()
        .expect("an unclaimed row may accept fully fenced lineage")
        .agent_id(),
        "00000000-0000-0000-0000-000000000002"
    );
}

/// A lineage that cannot become a path fails **the whole request**; it never degrades into "run
/// as usual with no lineage".
///
/// Degrading lets the session run all the way to the Stop hook, where the same string builds
/// `~/.agit/repos/<owner>/<name>` — by then it is another process, and this side can no longer
/// stop it.
#[test]
fn a_lineage_that_cannot_become_a_path_is_refused_at_the_wire_not_downgraded() {
    for (a, b) in [
        (Some("../.."), Some("main")),
        (Some("a/.."), Some("main")),
        (Some("/etc/passwd"), Some("main")),
        (Some("acme/api"), Some("-x")),
        (Some("acme/api"), Some("--upload-pack=touch /tmp/pwn")),
        (Some("acme/api "), Some("main")),
    ] {
        let err = lineage_from_params(true, a, Some("00000000-0000-0000-0000-000000000001"), b)
            .expect_err(&format!("{a:?} {b:?} must be refused"));
        assert_eq!(err.code, ErrorCode::PathNotAllowed as i32, "{a:?} {b:?}");
    }
}

#[test]
fn resume_location_consumes_one_scan_and_opens_no_transcript_for_gist() {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let mut mirror = Mirror::default();
    mirror
        .bind("ws", "project", &root)
        .expect("test project is bindable");
    let gist_calls = std::cell::Cell::new(0usize);
    let scanned = finish_local_sessions(
        vec![LocalSession {
            runtime_session_id: "native-thread".into(),
            runtime: "claude-code".into(),
            cwd: root.to_string_lossy().to_string(),
            modified_at: "2026-08-22T00:00:00Z".into(),
            gist: None,
            adopted: false,
            agent: None,
            likely_active: true,
        }],
        LocalSessionScan::Locate,
        |_| {
            gist_calls.set(gist_calls.get() + 1);
            Some("should never be parsed".into())
        },
    );
    assert_eq!(gist_calls.get(), 0, "resume lookup must not parse a gist");

    let scans = std::cell::Cell::new(0usize);
    let located = locate_local_with(&mirror, "ws", "native-thread", || {
        scans.set(scans.get() + 1);
        scanned
    })
    .unwrap();
    assert_eq!(scans.get(), 1, "one resume request gets one local snapshot");
    assert_eq!(located.runtime, "claude-code");
    assert_eq!(located.cwd, root);
    assert_eq!(located.project_id.as_deref(), Some("project"));
    assert!(
        located.likely_active,
        "the busy guard and launch coordinates must use the same row"
    );
}

/// The cost of listing local sessions must scale with how many sessions this project holds, not
/// with the total number of sessions on disk. The adapter layer already indexes by cwd (codex's
/// `threads` table, one readdir for Claude Code); the truncation here backstops the fallback
/// path taken when that index is unavailable.
// Both sides of the assertion are constants, so clippy reads it as always true — which is
// exactly the point: it guards the **relation between the constants**, and turns red on the spot
// when someone raises `PER_PROJECT_LIMIT` or pushes `LOCAL_GIST_BUDGET` too far. Deleting it
// hands that relation back to being enforced by remembering.
#[allow(clippy::assertions_on_constants)]
#[test]
fn the_per_project_cap_bounds_the_worst_case() {
    // The codex fallback path returns **every** rollout on disk, a number that grows without
    // bound with how long this machine has been in use. After truncation the worst case is
    // still a short list.
    assert_eq!(PER_PROJECT_LIMIT, 50);
    assert!(PER_PROJECT_LIMIT < 20_000);
    // The gist budget is separate and smaller — it is the only place that actually opens a
    // transcript.
    assert!(LOCAL_GIST_BUDGET <= PER_PROJECT_LIMIT);
}

#[test]
fn rfc3339_is_sortable_lexicographically() {
    let older = rfc3339(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000));
    let newer = rfc3339(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_800_000_000));
    // The list sorts by string in reverse, so this property is a precondition for correct order.
    assert!(newer > older, "{newer} must sort after {older}");
}
