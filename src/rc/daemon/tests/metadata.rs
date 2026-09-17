use super::*;

#[tokio::test]
async fn metadata_does_not_block_input_or_release_its_writer_guard() {
    for method_name in [method::SESSION_COMMANDS, method::SESSION_MODEL] {
        assert!(super::super::session_metadata::is_session_metadata(
            method_name
        ));
        assert!(!is_queued_session_rpc(method_name));
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
        let mut metadata =
            Frame::request(method_name, serde_json::json!({"session_id":"session-a"}));
        metadata.caller = Some(claim("owner", "another-workspace"));
        assert!(
            daemon
                .lock()
                .await
                .prepare_session_metadata(&metadata)
                .is_err()
        );
        metadata.caller = Some(claim("owner", "ws-a"));
        let prepared = daemon
            .lock()
            .await
            .prepare_session_metadata(&metadata)
            .unwrap();
        let worker_daemon = daemon.clone();
        let (_stop_tx, mut stop) = tokio::sync::watch::channel(false);
        let worker = tokio::spawn(async move { prepared.execute(worker_daemon, &mut stop).await });
        let metadata_reply = match rx.recv().await.unwrap() {
            Command::Runtime { name, reply, .. } => {
                assert_eq!(name, "commands");
                reply
            }
            Command::Model { model, reply } => {
                assert!(model.is_none());
                reply
            }
            _ => panic!("metadata must enqueue only a read-only command"),
        };
        assert!(metadata_reply.accept());
        let turn = daemon
            .lock()
            .await
            .prepare_session_rpc(&rpc_turn_frame("session-a"))
            .unwrap();
        let worker_daemon = daemon.clone();
        let (_turn_stop_tx, mut stop) = tokio::sync::watch::channel(false);
        let turn_worker = tokio::spawn(async move { turn.execute(worker_daemon, &mut stop).await });
        let turn_reply = match rx.recv().await.unwrap() {
            Command::Turn { reply, .. } => reply,
            _ => panic!("input must reach the queue while metadata is outstanding"),
        };
        daemon
            .lock()
            .await
            .sessions
            .get_mut("session-a")
            .unwrap()
            .rpc_guard_sensitive = true;
        metadata_reply.finish(Ok(serde_json::json!({"ready":true})));
        assert_eq!(
            worker.await.unwrap().unwrap(),
            serde_json::json!({"ready":true})
        );
        {
            let mut state = daemon.lock().await;
            assert!(state.sessions["session-a"].rpc_guard_sensitive);
            assert!(state.sessions["session-a"].rpc_gate.try_lock().is_err());
            assert!(
                state
                    .prepare_session_rpc(&rpc_turn_frame("session-a"))
                    .is_err()
            );
        }
        assert!(turn_reply.accept());
        turn_reply.finish(Ok(TurnStartOutcome::Accepted {
            turn_id: "accepted-turn".into(),
            still_running: true,
            consumed_mode: None,
            confirmation: TurnStartConfirmation::Exact,
        }));
        assert!(turn_worker.await.unwrap().response.is_ok());
    }
}
