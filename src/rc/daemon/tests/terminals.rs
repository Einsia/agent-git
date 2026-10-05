use super::*;

#[test]
fn terminal_open_replays_only_the_same_live_shell_and_instance() {
    if crate::rc::in_isolated_test(
        "rc::daemon::tests::terminals::terminal_open_replays_only_the_same_live_shell_and_instance",
    ) {
        return;
    }
    // Interactive profile and terminal negotiation must not control the protocol fixture.
    unsafe { std::env::set_var("SHELL", "/bin/sh") };

    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(home.path(), || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let daemon = rpc_test_daemon(HashMap::new(), Roster::default());
                let mut daemon = daemon.lock().await;
                daemon
                    .mirror
                    .bind("ws-a", "project", project.path())
                    .unwrap();
                let (frames, mut rx) = mpsc::channel(2048);
                let params = serde_json::json!({
                    "workspace_id":"ws-a", "project_id":"project",
                    "open_id":uuid::Uuid::new_v4().to_string(),
                    "instance_id":daemon.identity.instance_id,
                    "cols":80, "rows":24,
                });
                let request = |params: serde_json::Value| {
                    let mut frame = Frame::request(method::TERMINAL_OPEN, params);
                    frame.caller = Some(claim("owner", "ws-a"));
                    frame
                };
                let first = daemon
                    .dispatch(&request(params.clone()), &frames)
                    .await
                    .unwrap();
                let id = first["terminal_id"].as_str().unwrap();
                let original_pid = shell_pid(&daemon, id, &mut rx).await;
                // Delivery backpressure must not refuse an already admitted shell.
                daemon
                    .terminal_delivery_blockers
                    .store(1, std::sync::atomic::Ordering::SeqCst);
                let replay = daemon
                    .dispatch(&request(params.clone()), &frames)
                    .await
                    .unwrap();
                assert_eq!(replay, first);
                assert_eq!(shell_pid(&daemon, id, &mut rx).await, original_pid);
                assert_eq!(daemon.terminals.len(), 1);
                daemon
                    .terminal_delivery_blockers
                    .store(0, std::sync::atomic::Ordering::SeqCst);
                let mut changed = params.clone();
                changed["cols"] = serde_json::json!(120);
                assert!(
                    daemon
                        .dispatch(&request(changed), &frames)
                        .await
                        .unwrap_err()
                        .is(ErrorCode::MalformedFrame)
                );
                let mut outsider = request(params.clone());
                outsider.caller = Some(claim("owner", "ws-b"));
                assert!(daemon.dispatch(&outsider, &frames).await.is_err());
                let mut close = Frame::request(
                    method::TERMINAL_CLOSE,
                    serde_json::json!({"terminal_id":id}),
                );
                close.caller = Some(claim("owner", "ws-a"));
                daemon.dispatch(&close, &frames).await.unwrap();
                assert!(daemon.terminals.contains_key(id));
                assert!(
                    daemon
                        .terminal_owned_by(id, &claim("owner", "ws-a"))
                        .is_err()
                );
                assert!(
                    daemon
                        .dispatch(&request(params.clone()), &frames)
                        .await
                        .unwrap_err()
                        .is(ErrorCode::SessionNotFound)
                );
                drop(daemon.terminals.remove(id));
                daemon.identity.instance_id = uuid::Uuid::new_v4().to_string();
                assert!(
                    daemon
                        .dispatch(&request(params), &frames)
                        .await
                        .unwrap_err()
                        .is(ErrorCode::SessionNotFound)
                );
                assert!(daemon.terminals.is_empty());
            });
    });
}

async fn shell_pid(daemon: &Daemon, id: &str, frames: &mut mpsc::Receiver<Frame>) -> u32 {
    daemon.terminals[id]
        .term
        .write("sh -c 'printf \"\\036AGIT_PID=%s\\037\\n\" \"$PPID\"'\n")
        .unwrap();
    let mut output = String::new();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let frame = frames.recv().await.unwrap();
            if frame.method() == method::TERMINAL_OUTPUT
                && frame.params.as_ref().unwrap()["terminal_id"] == id
            {
                output.push_str(frame.params.as_ref().unwrap()["data"].as_str().unwrap());
                if let Some((_, tail)) = output.split_once('\x1e')
                    && let Some((pid, _)) =
                        tail.strip_prefix("AGIT_PID=").unwrap().split_once('\x1f')
                {
                    return pid.parse().unwrap();
                }
            }
        }
    })
    .await;
    result.unwrap_or_else(|_| panic!("the live shell must answer: {output:?}"))
}
