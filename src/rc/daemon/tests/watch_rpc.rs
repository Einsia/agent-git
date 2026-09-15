use super::*;
use crate::rc::daemon::tests::{rpc_test_daemon, rpc_test_live};
use std::future::Future;
use std::task::Poll;
use std::time::Duration;

async fn ready<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("watch operation must make progress")
}

fn request(method: &str, workspace: &str, session: &str) -> Frame {
    let mut frame = Frame::request(
        method,
        SessionWatch {
            workspace_id: workspace.into(),
            session_id: session.into(),
        },
    );
    frame.caller = Some(crate::protocol::CallerClaim {
        account_id: Some("viewer".into()),
        username: Some("viewer".into()),
        role: "viewer".into(),
        workspace_id: workspace.into(),
    });
    frame
}

async fn fixture(root: &Path) -> Arc<Mutex<Daemon>> {
    let daemon = rpc_test_daemon(HashMap::new(), Roster::default());
    daemon
        .lock()
        .await
        .mirror
        .bind("ws", "project", root)
        .unwrap();
    daemon
}

fn prepared(scan: WatchScan, cwd: PathBuf) -> PreparedWatch {
    PreparedWatch {
        request: scan.request,
        roots: scan.snapshot.roots,
        runtime: "codex".into(),
        seed: None,
        source: WatchSource::File {
            path: cwd.join("history.jsonl"),
            offset: 0,
            handle: None,
        },
        cwd,
        from_line: 0,
        total_lines: 0,
        absolute_lines: true,
        before_cursor: 0,
    }
}

async fn response(rx: &mut crate::rc::outbound::OutboundRx) -> Frame {
    let pending = ready(rx.next_write()).await.unwrap();
    let frame = pending.frame().clone();
    pending.commit();
    frame
}

#[tokio::test]
async fn cancelled_intermediate_request_keeps_stream_order_and_admission() {
    let mut queue = WatchRpcQueue::default();
    let frame = request(method::SESSION_WATCH, "ws", "native");
    let first = queue.reserve(&frame).unwrap();
    let middle = queue.reserve(&frame).unwrap();
    let mut last = queue.reserve(&frame).unwrap();
    drop(middle);
    let mut waiting = Box::pin(barrier(&mut last.previous));
    std::future::poll_fn(|cx| {
        assert!(waiting.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(queue.requests.available_permits(), MAX_WATCH_REQUESTS - 3);
    for (workspace, session) in [("other", "native"), ("ws", "another")] {
        let mut independent = queue
            .reserve(&request(method::SESSION_UNWATCH, workspace, session))
            .unwrap();
        ready(barrier(&mut independent.previous)).await;
    }
    drop(first);
    ready(waiting).await;
    drop(last);
    let permits = ready(
        queue
            .requests
            .clone()
            .acquire_many_owned(MAX_WATCH_REQUESTS as u32),
    )
    .await
    .unwrap();
    drop(permits);
    let tickets: Vec<_> = (0..MAX_WATCH_REQUESTS)
        .map(|_| queue.reserve(&frame).unwrap())
        .collect();
    assert_eq!(
        queue.reserve(&frame).err().unwrap().code,
        ErrorCode::SessionBusy as i32
    );
    drop(tickets);
}

#[tokio::test]
async fn slow_scan_releases_daemon_and_unwatch_cannot_overtake_it() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    let daemon = fixture(&cwd).await;
    let mut queue = WatchRpcQueue::default();
    let (outbound, mut replies) = crate::rc::outbound::channel();
    let (frames, _received) = mpsc::channel(8);
    let (_stop, stopping) = tokio::sync::watch::channel(false);
    let (started, scanning) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let watch = request(method::SESSION_WATCH, "ws", "native");
    let unwatch = request(method::SESSION_UNWATCH, "ws", "native");
    let opener = tokio::spawn(queue.reserve(&watch).unwrap().serve_with(
        daemon.clone(),
        outbound.clone(),
        frames.clone(),
        (watch.clone(), 0),
        stopping.clone(),
        move |scan| {
            started.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(prepared(scan, cwd))
        },
    ));
    ready(scanning).await.unwrap();
    {
        let mut state = ready(daemon.lock()).await;
        let probe = request(method::SESSION_UNWATCH, "ws", "independent");
        assert!(ready(state.dispatch(&probe, &frames)).await.is_ok());
        assert!(state.watches.is_empty());
    }
    let closer = tokio::spawn(queue.reserve(&unwatch).unwrap().serve(
        daemon.clone(),
        outbound,
        frames,
        unwatch.clone(),
        0,
        stopping,
    ));
    release.send(()).unwrap();
    ready(opener).await.unwrap();
    ready(closer).await.unwrap();
    let opened = response(&mut replies).await;
    assert_eq!(opened.id, watch.id);
    assert!(opened.error.is_none(), "{opened:?}");
    let closed = response(&mut replies).await;
    assert_eq!(closed.id, unwatch.id);
    assert!(closed.error.is_none());
    assert!(daemon.lock().await.watches.is_empty());
}

#[tokio::test]
async fn cancelled_scans_keep_the_blocking_budget_until_the_worker_exits() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    let daemon = fixture(&cwd).await;
    let mut queue = WatchRpcQueue::default();
    let (outbound, _replies) = crate::rc::outbound::channel();
    let (frames, _received) = mpsc::channel(8);
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let mut releases = Vec::new();
    let mut workers = Vec::new();
    for index in 0..MAX_WATCH_SCANS {
        let (started, scanning) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let frame = request(method::SESSION_WATCH, "ws", &format!("native-{index}"));
        let cwd = cwd.clone();
        workers.push(tokio::spawn(queue.reserve(&frame).unwrap().serve_with(
            daemon.clone(),
            outbound.clone(),
            frames.clone(),
            (frame, 0),
            stopping.clone(),
            move |scan| {
                started.send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(prepared(scan, cwd))
            },
        )));
        ready(scanning).await.unwrap();
        releases.push(release);
    }
    assert_eq!(queue.scans.available_permits(), 0);
    stop.send_replace(true);
    for worker in workers {
        ready(worker).await.unwrap();
    }
    assert_eq!(queue.scans.available_permits(), 0);
    assert!(daemon.lock().await.watches.is_empty());
    for release in releases {
        release.send(()).unwrap();
    }
    let permits = ready(
        queue
            .scans
            .clone()
            .acquire_many_owned(MAX_WATCH_SCANS as u32),
    )
    .await
    .unwrap();
    drop(permits);
    assert!(daemon.lock().await.watches.is_empty());
}

#[tokio::test]
async fn completed_scan_revalidates_connection_and_bound_folders() {
    for change_epoch in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().canonicalize().unwrap();
        let daemon = fixture(&cwd).await;
        let mut queue = WatchRpcQueue::default();
        let (outbound, mut replies) = crate::rc::outbound::channel();
        let (frames, _received) = mpsc::channel(8);
        let (_stop, stopping) = tokio::sync::watch::channel(false);
        let (started, scanning) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let frame = request(method::SESSION_WATCH, "ws", "native");
        let worker = tokio::spawn(queue.reserve(&frame).unwrap().serve_with(
            daemon.clone(),
            outbound,
            frames,
            (frame, 0),
            stopping,
            move |scan| {
                started.send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(prepared(scan, cwd))
            },
        ));
        ready(scanning).await.unwrap();
        {
            let mut state = daemon.lock().await;
            if change_epoch {
                state.settlement.send_modify(|state| state.epoch += 1);
            } else {
                state.mirror.unbind("ws", "project");
            }
        }
        release.send(()).unwrap();
        ready(worker).await.unwrap();
        assert!(daemon.lock().await.watches.is_empty());
        if change_epoch {
            assert!(ready(replies.next_write()).await.is_none());
        } else {
            assert_eq!(
                response(&mut replies).await.error.unwrap().code,
                ErrorCode::WorkspaceNotFound as i32
            );
        }
    }
}

#[tokio::test]
async fn installation_rechecks_identity_supervision_and_viewer_counts() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    let daemon = fixture(&cwd).await;
    let (frames, _received) = mpsc::channel(8);
    let frame = request(method::SESSION_WATCH, "ws", "native");
    let mut state = daemon.lock().await;
    let wrong = prepared(state.prepare_watch_scan(&frame).unwrap(), cwd.clone());
    let other = request(method::SESSION_WATCH, "ws", "other");
    assert_eq!(
        state
            .finish_watch_scan(&other, wrong, &frames)
            .unwrap_err()
            .code,
        ErrorCode::MalformedFrame as i32
    );
    let waiting = prepared(state.prepare_watch_scan(&frame).unwrap(), cwd.clone());
    let (tx, _commands) = mpsc::channel(1);
    let mut live = rpc_test_live(
        "supervised",
        1,
        tx,
        crate::protocol::PermissionMode::Default,
    );
    live.runtime_thread_id = Some("native".into());
    state.sessions.insert("supervised".into(), live);
    assert_eq!(
        state
            .finish_watch_scan(&frame, waiting, &frames)
            .unwrap_err()
            .code,
        ErrorCode::SessionBusy as i32
    );
    state.sessions.clear();
    for account in ["viewer", "another", "viewer"] {
        let mut frame = frame.clone();
        frame.caller.as_mut().unwrap().account_id = Some(account.into());
        let waiting = prepared(state.prepare_watch_scan(&frame).unwrap(), cwd.clone());
        state.finish_watch_scan(&frame, waiting, &frames).unwrap();
    }
    let stream = watch_stream_id("ws", "native");
    assert_eq!(state.watches.len(), 1);
    assert_eq!(state.watches[&stream].viewers["viewer"], 2);
    assert_eq!(state.watches[&stream].viewers["another"], 1);
    for account in ["viewer", "another", "viewer"] {
        let mut frame = request(method::SESSION_UNWATCH, "ws", "native");
        frame.caller.as_mut().unwrap().account_id = Some(account.into());
        state.dispatch(&frame, &frames).await.unwrap();
    }
    assert!(state.watches.is_empty());
}

#[test]
fn native_watch_reuses_readonly_snapshot_without_materializing_exports() {
    const CHILD: &str = "AGIT_RC_WATCH_SNAPSHOT_TEST_CHILD";
    let Some(root) = std::env::var_os(CHILD).map(PathBuf::from) else {
        let directory = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "rc::daemon::watch_rpc::tests::native_watch_reuses_readonly_snapshot_without_materializing_exports",
                "--nocapture",
            ])
            .env(CHILD, directory.path())
            .env("AGIT_HOME", directory.path().join("agit"))
            .env("XDG_DATA_HOME", directory.path().join("data"))
            .output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(directory.path().join("completed").exists());
        return;
    };
    let cwd = root.canonicalize().unwrap();
    let native = root.join("data/opencode");
    std::fs::create_dir_all(&native).unwrap();
    let database = rusqlite::Connection::open(native.join("opencode.db")).unwrap();
    database.execute_batch(
        r#"CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT);
        CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT,
            directory TEXT, time_created INTEGER, time_updated INTEGER, version TEXT);
        CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
        CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);
        INSERT INTO message VALUES ('message', 'ses_watch', 1, '{"role":"user"}');
        INSERT INTO part VALUES ('prompt', 'message', 'ses_watch', 2, '{"type":"text","text":"Snapshot before the source changed"}');"#,
    ).unwrap();
    database
        .execute(
            "INSERT INTO project VALUES ('project', ?1)",
            [&cwd.to_string_lossy()],
        )
        .unwrap();
    database
        .execute(
            "INSERT INTO session VALUES ('ses_watch', 'project', NULL, ?1, 1, 2, 'fixture')",
            [&cwd.to_string_lossy()],
        )
        .unwrap();
    use crate::adapter::{Adapter, opencode::OpenCode};
    let refs = OpenCode.sessions_for(&cwd).unwrap();
    assert_eq!(refs.len(), 1);
    let cache = refs[0].path.clone();
    let local = LocalSession {
        runtime_session_id: "ses_watch".into(),
        runtime: "opencode".into(),
        cwd: cwd.to_string_lossy().into_owned(),
        modified_at: "now".into(),
        gist: None,
        adopted: false,
        agent: None,
        likely_active: false,
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let daemon = fixture(&cwd).await;
            let frame = request(method::SESSION_WATCH, "ws", "ses_watch");
            let prepare = || {
                WatchScan::prepare_source(
                    frame.params_as().unwrap(),
                    policy::CanonicalRoots::from_untrusted([cwd.clone()]),
                    local.clone(),
                )
                .unwrap()
            };
            let first = prepare();
            assert!(!cache.exists());
            assert!(matches!(first.source, WatchSource::Native { .. }));
            std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
            std::fs::write(&cache, "saved export").unwrap();
            let prepared = prepare();
            assert_eq!(std::fs::read_to_string(&cache).unwrap(), "saved export");
            assert!(prepared.absolute_lines);
            assert!(prepared.total_lines > 0);
            database.execute("DELETE FROM part", []).unwrap();
            let (frames, mut received) = mpsc::channel(8);
            daemon
                .lock()
                .await
                .finish_watch_scan(&frame, prepared, &frames)
                .unwrap();
            let replay = ready(received.recv()).await.unwrap();
            let item: crate::protocol::ItemCompleted = replay.params_as().unwrap();
            assert_eq!(
                item.event.text.as_deref(),
                Some("Snapshot before the source changed")
            );
            let unwatch = request(method::SESSION_UNWATCH, "ws", "ses_watch");
            daemon
                .lock()
                .await
                .dispatch(&unwatch, &frames)
                .await
                .unwrap();
            let source = OpenCode
                .lookup_native_readonly("ses_watch", Default::default())
                .unwrap();
            let elsewhere = tempfile::tempdir().unwrap();
            database
                .execute(
                    "UPDATE session SET directory = ?1",
                    [&elsewhere.path().to_string_lossy()],
                )
                .unwrap();
            assert!(
                crate::rc::supervisor::native_records::read_watch_snapshot_blocking(&source, &cwd)
                    .is_err()
            );
        });
    std::fs::write(root.join("completed"), []).unwrap();
}
