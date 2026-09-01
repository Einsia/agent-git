use super::*;

#[cfg(unix)]
fn touch(path: &std::path::Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("touch");
    command.arg(path);
    command
}

#[cfg(unix)]
fn exit(status: i32) -> std::process::Output {
    std::process::Command::new("sh")
        .args(["-c", &format!("exit {status}")])
        .output()
        .unwrap()
}

/// The supervisor must distinguish a real new commit from the hook-style
/// success/no-op convention, and it must not publish a settlement until
/// the push itself returned success. A failed push leaves the exact new
/// SHA pending so the next idempotent attempt can recover it.
#[cfg(unix)]
#[test]
fn strict_settlement_requires_a_new_commit_and_a_confirmed_push() {
    let ok = exit(0);
    let failed = exit(7);

    assert!(strict_settlement_candidate("old", &failed, "new", Some("new"), None).is_err());
    assert_eq!(
        strict_settlement_candidate("same", &ok, "same", None, None).unwrap(),
        None,
        "a successful no-op must not push an old HEAD"
    );
    assert!(
        strict_settlement_candidate("old", &ok, "new", None, None).is_err(),
        "a concurrently moved HEAD is not proof that this command settled the session"
    );

    let pending = strict_settlement_candidate("old", &ok, "new", Some("new"), None)
        .unwrap()
        .expect("a changed HEAD is the strict settlement candidate");
    assert_eq!(pending, "new");
    assert_eq!(
        confirmed_strict_push(&failed, &pending),
        None,
        "a failed push emits no commit.settled"
    );

    let retry = strict_settlement_candidate("new", &ok, "new", None, Some(&pending))
        .unwrap()
        .expect("the next no-op retries the unconfirmed push");
    assert_eq!(retry, pending);
    assert_eq!(
        confirmed_strict_push(&ok, &retry),
        Some("new".to_string()),
        "a confirmed retry emits the pending SHA exactly once"
    );
    assert!(
        strict_settlement_candidate("new", &ok, "new", None, None)
            .unwrap()
            .is_none(),
        "clearing the pending SHA after notification prevents duplicates"
    );
}

/// Guards against "the predicate is right and the inputs are the test's own invention": the
/// test above feeds only hand-written "old"/"new". Here the **guarded subprocess** really runs
/// `git rev-parse HEAD` and hands its stdout to the predicate unchanged — without
/// `Stdio::piped()` in `guarded_output`, tokio lets the child inherit stdio, `Output.stdout` is
/// forever the empty string while the status is success, so every settlement is judged
/// "strict commit left an unreadable HEAD" and not one `commit.settled` goes out, while the
/// unit tests that feed hand-written input alone stay **all green**. Delete the piped lines
/// from `guarded_output` and this goes red at once.
#[cfg(unix)]
#[tokio::test]
async fn the_guarded_subprocess_feeds_a_real_head_to_the_settlement_predicate() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().to_str().unwrap().to_string();
    let git = |args: &[&str]| {
        let mut command = tokio::process::Command::new("git");
        command
            .args(crate::domain::meta::GIT_SAFE)
            .args(["-C", &repo])
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("HOME", &repo);
        command
    };
    // The same command and the same guarded channel as `read_head` in `settle_and_push`.
    let read_head = || {
        let mut head = tokio::process::Command::new("git");
        head.args(crate::domain::meta::GIT_SAFE)
            .args(["-C", &repo, "rev-parse", "HEAD"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0");
        head
    };

    let (_tx, mut rx) = tokio::sync::watch::channel(SettlementState {
        epoch: 1,
        agent_identity_v1: true,
        session_start_idempotency_v1: false,
    });
    let lease = *rx.borrow_and_update();

    let init = guarded_output(&mut rx, lease, git(&["init", "-q"]))
        .await
        .expect("lease is current");
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let first = guarded_output(
        &mut rx,
        lease,
        git(&["commit", "-q", "--allow-empty", "-m", "before"]),
    )
    .await
    .expect("lease is current");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );

    let before = guarded_output(&mut rx, lease, read_head())
        .await
        .expect("lease is current");
    assert!(before.status.success());
    let before = String::from_utf8_lossy(&before.stdout).trim().to_string();
    assert_eq!(
        before.len(),
        40,
        "the guarded child's stdout must carry the real HEAD, got `{before}`"
    );

    // Stands in for the step where `agit commit --from-supervisor` produces a new commit.
    let commit = guarded_output(
        &mut rx,
        lease,
        git(&["commit", "-q", "--allow-empty", "-m", "turn"]),
    )
    .await
    .expect("lease is current");
    let after = guarded_output(&mut rx, lease, read_head())
        .await
        .expect("lease is current");
    assert!(after.status.success());
    let after = String::from_utf8_lossy(&after.stdout).trim().to_string();
    assert_eq!(after.len(), 40, "the settlement HEAD read back empty");
    assert_ne!(before, after, "the guarded commit did not move HEAD");

    let candidate = strict_settlement_candidate(&before, &commit, &after, Some(&after), None)
        .expect("real subprocess outputs must satisfy the settlement predicate")
        .expect("a really-new HEAD is the settlement candidate");
    assert_eq!(candidate, after);
}

/// Regression (no settlement at session end): when `Command::Shutdown` / driver EOF / `Exited`
/// return directly, `settle_and_push` never runs — the pending settlement a failed push left
/// behind is dropped along with the Session, the commit is local and the hub waits forever for
/// `commit.settled`. The probe is a delivery from an **old epoch**: it is judged stale and
/// invalidated only if the exit path really entered `settle_and_push`. Delete the
/// `settle_on_exit` call in `run` and this assertion goes red (the probe stays Pending).
#[tokio::test]
async fn session_end_runs_a_final_settlement_for_the_pending_push() {
    let driver = AnyDriver::ClaudeCode(Box::new(
        crate::rc::harness::claude_code::ClaudeCodeDriver::test_driver(),
    ));
    let (mut session, _out, mut notes) = super::tests::harness_test_session_with_channels(
        driver,
        "claude-code",
        SessionStatus::Idle,
    );
    let lease = SettlementState {
        epoch: 3,
        agent_identity_v1: true,
        session_start_idempotency_v1: false,
    };
    let (_settlement_tx, settlement_rx) = tokio::sync::watch::channel(lease);
    session.settlement = settlement_rx;
    let probe = crate::protocol::ConnectionDelivery::new(
        2,
        crate::protocol::ConnectionFeature::AgentIdentityV1,
    );
    session.pending_settlement = Some(PendingSettlement {
        sha: "cafebabe".into(),
        delivery: Some(probe.clone()),
        receipt: None,
    });

    let (commands, commands_rx) = mpsc::channel(4);
    let worker = tokio::spawn(session.run(commands_rx));
    commands
        .send(Command::Shutdown)
        .await
        .expect("queue the user's stop");
    tokio::time::timeout(std::time::Duration::from_secs(5), worker)
        .await
        .expect("the session exits promptly")
        .expect("supervisor joins");
    assert_eq!(
        probe.status(),
        crate::protocol::DeliveryStatus::Stale,
        "the exit path never entered settle_and_push — a pending settlement dies with the session"
    );
    loop {
        match notes.recv().await.expect("the Ended note is mandatory") {
            SessionNote::Ended { session_id, .. } => {
                assert_eq!(session_id, "session-turn-test");
                break;
            }
            _ => continue,
        }
    }
}

/// The other half of what `settle_on_exit` owes: lines written into the transcript after the
/// last turn boundary still become `item.completed` at session end, rather than being lost
/// forever because no further `turn.completed` arrives.
#[tokio::test]
async fn exit_flush_drains_transcript_lines_written_after_the_last_turn() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.jsonl");
    std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"the final reply"}]},"sessionId":"s"}"#,
                "
"
            ),
        )
        .unwrap();
    let driver = AnyDriver::ClaudeCode(Box::new(
        crate::rc::harness::claude_code::ClaudeCodeDriver::test_driver(),
    ));
    let (mut session, mut out, _notes) = super::tests::harness_test_session_with_channels(
        driver,
        "claude-code",
        SessionStatus::Idle,
    );
    session.tailer = Some(Tailer::new(&path, true));

    session.settle_on_exit().await;

    let mut saw_item = false;
    while let Ok(frame) = out.try_recv() {
        if frame.method.as_deref() == Some(method::ITEM_COMPLETED) {
            saw_item = true;
        }
    }
    assert!(
        saw_item,
        "the exit flush must drain the tail of the transcript into item.completed"
    );
}

#[tokio::test]
async fn local_queueing_is_not_connection_delivery() {
    let lease = SettlementState {
        epoch: 7,
        agent_identity_v1: true,
        session_start_idempotency_v1: false,
    };
    let (state_tx, mut state_rx) = tokio::sync::watch::channel(lease);
    let delivery = crate::protocol::ConnectionDelivery::new(
        lease.epoch,
        crate::protocol::ConnectionFeature::AgentIdentityV1,
    );
    let (local_tx, mut local_rx) = tokio::sync::mpsc::channel(1);
    let mut frame = Frame::notification(method::COMMIT_SETTLED, serde_json::json!({}));
    frame.connection_delivery = Some(delivery.clone());
    local_tx.send(frame).await.unwrap();
    let _locally_queued = local_rx.recv().await.unwrap();

    {
        let waiting = wait_for_connection_delivery_within(
            &mut state_rx,
            lease,
            &delivery,
            std::time::Duration::from_secs(1),
        );
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), &mut waiting)
                .await
                .is_err(),
            "accepting the frame into a local mpsc must not clear pending settlement"
        );
        delivery.mark_delivered();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_millis(200), &mut waiting)
                .await
                .expect("the link write wakes the delivery waiter"),
            crate::protocol::DeliveryStatus::Delivered
        );
    }

    let stale = crate::protocol::ConnectionDelivery::new(
        lease.epoch,
        crate::protocol::ConnectionFeature::AgentIdentityV1,
    );
    state_tx.send_modify(|state| {
        state.epoch = 8;
        state.agent_identity_v1 = false;
    });
    assert_eq!(
        wait_for_connection_delivery_within(
            &mut state_rx,
            lease,
            &stale,
            std::time::Duration::from_millis(200),
        )
        .await,
        crate::protocol::DeliveryStatus::Stale,
        "losing the exact ACK epoch makes every queued clone unsendable"
    );
}

/// No explicit ACK means no settlement subprocess is even spawned. A
/// disconnect invalidates an in-flight lease; a later ACK creates a new
/// lease and the next settlement may perform commit and push exactly once.
#[cfg(unix)]
#[tokio::test]
async fn identity_ack_is_a_dynamic_kill_switch_for_commit_and_push() {
    let dir = tempfile::tempdir().unwrap();
    let commit = dir.path().join("commit-ran");
    let push = dir.path().join("push-ran");
    let (tx, mut rx) = tokio::sync::watch::channel(SettlementState::default());

    let unacked = SettlementState {
        epoch: 0,
        agent_identity_v1: true,
        session_start_idempotency_v1: false,
    };
    assert!(
        guarded_output(&mut rx, unacked, touch(&commit))
            .await
            .is_none()
    );
    assert!(
        guarded_output(&mut rx, unacked, touch(&push))
            .await
            .is_none()
    );
    assert!(!commit.exists() && !push.exists(), "zero writes before ACK");

    tx.send_modify(|state| {
        state.epoch = 1;
        state.agent_identity_v1 = true;
    });
    let lease = *rx.borrow_and_update();
    {
        let mut hanging = tokio::process::Command::new("sleep");
        hanging.arg("30");
        let running = guarded_output(&mut rx, lease, hanging);
        tokio::pin!(running);
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            _ = &mut running => panic!("the in-flight command unexpectedly finished"),
        }
        tx.send_modify(|state| {
            state.epoch = 2;
            state.agent_identity_v1 = false;
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), &mut running)
                .await
                .expect("lost ACK did not kill the subprocess")
                .is_none()
        );
    }
    assert!(
        !commit.exists() && !push.exists(),
        "zero writes after lost ACK"
    );

    tx.send_modify(|state| {
        state.epoch = 3;
        state.agent_identity_v1 = true;
    });
    let renewed = *rx.borrow_and_update();
    assert!(
        guarded_output(&mut rx, renewed, touch(&commit))
            .await
            .is_some()
    );
    assert!(
        guarded_output(&mut rx, renewed, touch(&push))
            .await
            .is_some()
    );
    assert!(
        commit.exists() && push.exists(),
        "the renewed lease settles once"
    );
}

/// The settlement child can itself be `agit`, with git/remote-helper
/// descendants below it. On Windows a direct-child kill leaves those
/// descendants running, so this recursive test process schedules a delayed
/// write in its grandchild and proves ACK loss tears down the whole Job.
#[cfg(windows)]
#[tokio::test]
async fn windows_job_kills_a_settlement_grandchild_before_guard_release() {
    const TEST: &str = "rc::supervisor::settle_tests::windows_job_kills_a_settlement_grandchild_before_guard_release";
    const MODE: &str = "AGIT_WINDOWS_JOB_TEST_MODE";
    const READY: &str = "AGIT_WINDOWS_JOB_TEST_READY";
    const MARKER: &str = "AGIT_WINDOWS_JOB_TEST_MARKER";

    match std::env::var(MODE).as_deref() {
        Ok("grandchild") => {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            std::fs::write(std::env::var_os(MARKER).unwrap(), b"survived").unwrap();
            return;
        }
        Ok("parent") => {
            let mut descendant = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(MODE, "grandchild")
                .spawn()
                .unwrap();
            std::fs::write(
                std::env::var_os(READY).unwrap(),
                descendant.id().to_string(),
            )
            .unwrap();
            let _ = descendant.wait();
            return;
        }
        _ => {}
    }

    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("descendant-ready");
    let marker = dir.path().join("descendant-write");
    let (state_tx, mut state_rx) = tokio::sync::watch::channel(SettlementState {
        epoch: 1,
        agent_identity_v1: true,
        session_start_idempotency_v1: false,
    });
    let lease = *state_rx.borrow_and_update();
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", TEST, "--nocapture"])
        .env(MODE, "parent")
        .env(READY, &ready)
        .env(MARKER, &marker);
    let mut running = Box::pin(guarded_output(&mut state_rx, lease, command));

    let ready_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < ready_deadline,
            "settlement grandchild did not start"
        );
        tokio::select! {
            result = &mut running => panic!("settlement tree exited before ACK loss: {result:?}"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                if ready.exists() {
                    break;
                }
            }
        }
    }

    state_tx.send_modify(|state| {
        state.epoch = 2;
        state.agent_identity_v1 = false;
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut running)
            .await
            .expect("Job tree was not terminated and reaped")
            .is_none()
    );
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert!(
        !marker.exists(),
        "the grandchild survived guard release and performed its delayed write"
    );
}

/// The model's final reply may land **after** `turn.completed`: at the moment the harness emits
/// `result` on stdout, the last transcript line is not persisted yet. The quiet window must be
/// several poll intervals wide, or a client that stops at `turn.completed` drops the most
/// important sentence of the whole turn.
// As elsewhere: both sides are constants, so clippy judges the assertion always true, while the
// relation between them is exactly what it guards.
// Narrow `SETTLE_MAX_MS` to less than a few poll intervals and this assertion goes red.
#[allow(clippy::assertions_on_constants)]
#[test]
fn the_settle_window_is_several_poll_intervals_wide() {
    assert!(
        SETTLE_MAX_MS >= TAIL_POLL_MS * 8,
        "the settle window must be several poll intervals wide; only {} fit in {SETTLE_MAX_MS}ms",
        SETTLE_MAX_MS / TAIL_POLL_MS
    );
    // But not an unbounded wait — the harness may already be dead.
    assert!(SETTLE_MAX_MS <= 5_000);
}

/// One **real** settlement: a real git repo, a real bare remote, a real subprocess, real
/// remote-tracking refs.
///
/// The subprocess is not `agit` itself — `cargo test --lib` does not build that binary at all,
/// so a test pointing at it must fail under the project's standard test command. It is a script
/// that really runs `git commit` / `git push`: the half where `agit commit --from-supervisor`
/// obeys the result-file contract is pinned separately by `tests/rc_supervised_settlement.rs`
/// with the real binary; what is pinned here is the other half — the supervisor's own wiring
/// (the temporary file under `.git` → `SUPERVISOR_RESULT_ENV` → read back → the predicate →
/// the push → `commit.settled` really leaving `self.out` carrying that sha). This whole chain
/// can be dead while every unit test is green.
#[cfg(unix)]
struct SettlementFixture {
    _dir: tempfile::TempDir,
    repo: std::path::PathBuf,
    exe: std::path::PathBuf,
    branch: String,
}

/// The git the test runs itself uses the same `GIT_SAFE` and the same environment isolation as
/// the code under test.
#[cfg(unix)]
fn fixture_git(args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(crate::domain::meta::GIT_SAFE)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[cfg(unix)]
impl SettlementFixture {
    /// `new_turn` = whether this run of the subprocess produces a new commit (and writes the
    /// result file as the contract requires). false is "no new transcript" — the only occasion
    /// on which a pending settlement can still be pushed.
    fn new(new_turn: bool) -> SettlementFixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let bare = dir.path().join("bare.git");
        let branch = "s/settlement".to_string();
        std::fs::create_dir_all(&repo).unwrap();
        let git = fixture_git;
        let repo_s = repo.to_string_lossy().into_owned();
        let bare_s = bare.to_string_lossy().into_owned();
        git(&["init", "-q", "--bare", &bare_s]);
        git(&["init", "-q", "-b", &branch, &repo_s]);
        git(&["-C", &repo_s, "commit", "-q", "--allow-empty", "-m", "base"]);
        git(&["-C", &repo_s, "remote", "add", "origin", &bare_s]);
        // This push succeeds, so refs/remotes/origin/<branch> stops at base — one more local
        // step from there is the real shape of "the hub did not take it".
        git(&["-C", &repo_s, "push", "-q", "origin", &branch]);

        let exe = dir.path().join("settlement-child.sh");
        let commit_body = if new_turn {
            format!(
                "    git {safe} -c user.name=t -c user.email=t@t -c commit.gpgsign=false \
                     -C {repo} commit -q --allow-empty -m turn\n    \
                     git {safe} -C {repo} rev-parse HEAD > \"${result}\"\n",
                safe = crate::domain::meta::GIT_SAFE.join(" "),
                repo = repo_s,
                result = crate::commands::commit::SUPERVISOR_RESULT_ENV,
            )
        } else {
            // No new transcript: the real `agit commit --from-supervisor` exits successfully
            // and **does not write** the result file here, leaving the predicate only pending
            // to go on.
            "    :\n".to_string()
        };
        std::fs::write(
            &exe,
            format!(
                "#!/bin/sh\nset -e\nexport GIT_CONFIG_NOSYSTEM=1\n\
                     export GIT_CONFIG_GLOBAL=/dev/null\nexport GIT_TERMINAL_PROMPT=0\n\
                     case \"$1\" in\ncommit)\n{commit_body}  ;;\n\
                     push)\n    git {safe} -C {repo} push -q origin {branch}\n  ;;\n\
                     esac\nexit 0\n",
                safe = crate::domain::meta::GIT_SAFE.join(" "),
                repo = repo_s,
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

        SettlementFixture {
            _dir: dir,
            repo,
            exe,
            branch,
        }
    }

    /// The shape of binding a new project for the first time: the agent repo on the hub is
    /// empty, `rc land` clones back a repo with no ref at all and then builds the local main
    /// file line itself; the session branch grows off it while HEAD still sits on main. No turn
    /// has run yet.
    fn new_local_main_line_only() -> SettlementFixture {
        let fixture = SettlementFixture::new(false);
        let repo = fixture.repo.to_string_lossy().into_owned();
        // An emptied remote = the agent repo was just created and holds nothing.
        fixture_git(&[
            "-C",
            &repo,
            "push",
            "-q",
            "--delete",
            "origin",
            &fixture.branch,
        ]);
        // The main file line is built locally, and HEAD sits on it.
        fixture_git(&["-C", &repo, "checkout", "-q", "-b", "main"]);
        fixture
    }

    fn head(&self) -> String {
        let out = std::process::Command::new("git")
            .args(crate::domain::meta::GIT_SAFE)
            .arg("-C")
            .arg(&self.repo)
            .args(["rev-parse", "HEAD"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Where the durable receipt on the notification side lands.
    fn receipt(&self) -> std::path::PathBuf {
        unacked_settlement_path(&self.repo, &self.branch)
    }

    fn tracking(&self) -> String {
        let out = std::process::Command::new("git")
            .args(crate::domain::meta::GIT_SAFE)
            .arg("-C")
            .arg(&self.repo)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("refs/remotes/origin/{}", self.branch))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A session with its wiring done: lineage points at this repo, and the settlement
    /// subprocess is the script above.
    fn session(
        &self,
    ) -> (
        Session,
        mpsc::Receiver<Frame>,
        mpsc::Receiver<SessionNote>,
        tokio::sync::watch::Sender<SettlementState>,
        SettlementState,
    ) {
        let driver = AnyDriver::ClaudeCode(Box::new(
            crate::rc::harness::claude_code::ClaudeCodeDriver::test_driver(),
        ));
        let (mut session, out, notes) = super::tests::harness_test_session_with_channels(
            driver,
            "claude-code",
            SessionStatus::Idle,
        );
        let lease = SettlementState {
            epoch: 1,
            agent_identity_v1: true,
            session_start_idempotency_v1: false,
        };
        let (tx, rx) = tokio::sync::watch::channel(lease);
        session.settlement = rx;
        session.agit_session = Some(
            crate::rc::lineage::AgitSession::new(
                "alice/photo",
                "6f35cd22-85ae-4ed5-a106-2d9069d6b346",
                &self.branch,
            )
            .expect("a usable lineage"),
        );
        session.settlement_child = Some(SettlementChild {
            exe: self.exe.clone(),
            repo_dir: self.repo.clone(),
        });
        (session, out, notes, tx, lease)
    }
}

/// What becomes of the link in a test: the hub confirms it took the frame, or the connection
/// goes first.
#[cfg(unix)]
#[derive(Clone, Copy)]
enum LinkAck {
    /// The hub took this frame.
    Delivered,
    /// The transport dropped before the hub confirmed — in the daemon's shutdown path
    /// (`link_task.abort()` first, then exit settlement), this is what becomes of the
    /// notification exit settlement sends.
    LostBeforeAck,
}

/// Run one settlement while also playing the link layer: every frame that arrives gets a
/// verdict per `ack`, or the `SETTLEMENT_DELIVERY_WAIT` delivery wait at the end hangs the test.
///
/// **What is awaited is the settlement finishing, not "some frame arrived".** Written the other
/// way round (`join!` with one side awaiting `out.recv()`, wrapped in a `timeout`): once
/// settlement returns midway — and this chain is dotted with `else { return }` — that side
/// never gets its frame, so every failure, whichever link broke, comes out as the same
/// `Elapsed(())` and only after burning the whole timeout; on a busy machine that becomes a
/// flake with no stated cause. Settlement returning ends the wait, and which link broke is for
/// the assertions to say.
#[cfg(unix)]
async fn settle_draining(
    session: &mut Session,
    out: &mut mpsc::Receiver<Frame>,
    boundary: SettlementBoundary,
) -> Vec<Frame> {
    settle_with_link(session, out, boundary, LinkAck::Delivered).await
}

#[cfg(unix)]
async fn settle_with_link(
    session: &mut Session,
    out: &mut mpsc::Receiver<Frame>,
    boundary: SettlementBoundary,
    ack: LinkAck,
) -> Vec<Frame> {
    let mut frames = Vec::new();
    {
        let mut settle = std::pin::pin!(session.settle_and_push(boundary));
        loop {
            tokio::select! {
                () = &mut settle => break,
                Some(frame) = out.recv() => {
                    if let Some(delivery) = frame.connection_delivery.clone() {
                        match ack {
                            LinkAck::Delivered => delivery.mark_delivered(),
                            LinkAck::LostBeforeAck => delivery.invalidate(),
                        }
                    }
                    frames.push(frame);
                }
            }
        }
    }
    while let Ok(frame) = out.try_recv() {
        frames.push(frame);
    }
    frames
}

/// The one `commit.settled` this settlement sent. Absent means absent — the frames that did go
/// out are listed, so the next person is not left guessing at `Elapsed(())`.
#[cfg(unix)]
fn only_settled_frame(frames: Vec<Frame>) -> CommitSettled {
    let methods: Vec<String> = frames
        .iter()
        .map(|f| f.method.clone().unwrap_or_default())
        .collect();
    let settled: Vec<CommitSettled> = frames
        .into_iter()
        .filter(|f| f.method.as_deref() == Some(method::COMMIT_SETTLED))
        .map(|f| serde_json::from_value(f.params.expect("commit.settled carries params")).unwrap())
        .collect();
    assert_eq!(
        settled.len(),
        1,
        "expected exactly one commit.settled; the settlement emitted {methods:?}"
    );
    settled.into_iter().next().unwrap()
}

/// End to end (the settlement success path): the temporary result file under `.git` →
/// `SUPERVISOR_RESULT_ENV` → the subprocess writes it back → `read_to_string` → the predicate →
/// a real push → `commit.settled`.
///
/// Cover the predicate alone and the before/after/reported fed to it are strings the test made
/// up itself. Any link breaking (the result file never being handed down, say) turns `reported`
/// into `None` and makes the predicate return
/// "HEAD changed without a strict transcript settlement result":
/// **every RC settlement silently stalls**, and the unit tests stay green all the same.
#[cfg(unix)]
#[tokio::test]
async fn a_real_settlement_pushes_and_reports_the_sha_its_child_wrote() {
    let fixture = SettlementFixture::new(true);
    let base = fixture.head();
    let (mut session, mut out, _notes, _tx, _lease) = fixture.session();

    let settled =
        only_settled_frame(settle_draining(&mut session, &mut out, SettlementBoundary::Turn).await);
    let head = fixture.head();
    assert_ne!(head, base, "the child never created the turn commit");
    assert_eq!(
        settled.commit_sha, head,
        "commit.settled must name the sha the child reported through the result file"
    );
    assert_eq!(
        fixture.tracking(),
        head,
        "the settlement never actually pushed the new commit"
    );
}

/// Regression (BACKLOG A2: a pending settlement is not durable): after a failed push the daemon
/// is hard-killed or loses power, and the `Session` that comes back from resume has
/// `pending_settlement` as `None` (both `launch` paths hard-code `None`). With no new turn
/// after that, the predicate returns on `None => Ok(None)`, that commit is never pushed again
/// and `commit.settled` never names it. The fix is not to "remember" but to derive: a local
/// HEAD that no origin remote ref can reach is one the hub has not taken.
///
/// Swap the `unpushed_local_head` path back to `self.pending_settlement` and this goes red at
/// once: not one `commit.settled` is sent.
#[cfg(unix)]
#[tokio::test]
async fn a_pending_settlement_that_died_with_the_daemon_is_rederived_from_git() {
    let fixture = SettlementFixture::new(false);
    // The scene the earlier session left behind: the commit is made, the push did not succeed.
    let out = std::process::Command::new("git")
        .args(crate::domain::meta::GIT_SAFE)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(["-c", "commit.gpgsign=false"])
        .arg("-C")
        .arg(&fixture.repo)
        .args(["commit", "-q", "--allow-empty", "-m", "unpushed turn"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success());
    let orphan = fixture.head();
    assert_ne!(
        fixture.tracking(),
        orphan,
        "the premise needs an unpushed commit"
    );

    let (mut session, mut out, _notes, _tx, _lease) = fixture.session();
    // A session brought back by resume has exactly this shape.
    assert!(session.pending_settlement.is_none());

    let settled =
        only_settled_frame(settle_draining(&mut session, &mut out, SettlementBoundary::Turn).await);
    assert_eq!(
        settled.commit_sha, orphan,
        "commit.settled must name the commit the previous daemon failed to push"
    );
    assert_eq!(
        fixture.tracking(),
        orphan,
        "the orphaned commit was still never pushed"
    );
}

/// Regression (a lost settlement notification): the push succeeds, the hub has not confirmed it
/// took `commit.settled`, and the daemon is gone.
///
/// No power loss is needed to reach this — **an ordinary shutdown has exactly this shape**: the
/// daemon's shutdown path runs `link_task.abort()` to tear down the transport first, then
/// `shutdown()` → exit settlement. Exit settlement then commits and pushes successfully with no
/// consumer left to deliver that notification.
///
/// After that the git side has nothing left to say: the remote-tracking ref advanced the moment
/// the push succeeded, and [`unpushed_local_head`] can only return `None` (asked for real
/// below). Take "the remote ref already contains HEAD" for "the hub already took the
/// notification" and this settlement event is lost for good.
///
/// The fix gives the notification a durable watermark of its **own**: write the receipt before
/// the push, delete it only on `Delivered`. Take out `record_unacked_settlement` /
/// `read_unacked_settlement` and this goes red at once: the second daemon sends not one
/// `commit.settled`.
#[cfg(unix)]
#[tokio::test]
async fn a_settlement_whose_notification_was_never_acked_is_redelivered_after_a_restart() {
    let fixture = SettlementFixture::new(false);
    let repo_s = fixture.repo.to_string_lossy().into_owned();
    // A turn the earlier session produced, still only local.
    fixture_git(&[
        "-C",
        &repo_s,
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "the last turn",
    ]);
    let turn = fixture.head();

    // The first daemon: the push succeeds and the notification goes out, but the transport is
    // gone before the hub confirms.
    {
        let (mut session, mut out, _notes, _tx, _lease) = fixture.session();
        let frames = settle_with_link(
            &mut session,
            &mut out,
            SettlementBoundary::SessionExit,
            LinkAck::LostBeforeAck,
        )
        .await;
        assert_eq!(
            only_settled_frame(frames).commit_sha,
            turn,
            "the premise needs a first settlement that really reported this commit"
        );
        assert_eq!(
            fixture.tracking(),
            turn,
            "the premise needs a push that really landed on the remote"
        );
    }
    // The process is gone, and `pending_settlement` goes with the memory.

    // And the git side has nothing left to say — after a successful push it only answers "the
    // hub took it".
    let lease = SettlementState {
        epoch: 1,
        agent_identity_v1: true,
        session_start_idempotency_v1: false,
    };
    let (_probe_tx, mut probe_rx) = tokio::sync::watch::channel(lease);
    assert!(
        unpushed_local_head(&mut probe_rx, lease, &repo_s, &turn)
            .await
            .is_none(),
        "the premise of this regression is that git reachability no longer knows"
    );

    // The second daemon: a restored session has exactly this shape.
    let (mut session, mut out, _notes, _tx, _lease) = fixture.session();
    assert!(session.pending_settlement.is_none());
    let settled =
        only_settled_frame(settle_draining(&mut session, &mut out, SettlementBoundary::Turn).await);
    assert_eq!(
        settled.commit_sha, turn,
        "commit.settled must name the commit whose notification the hub never acked"
    );

    // This time the hub took it: the receipt is destroyed and a third session does not send it
    // all over again.
    assert!(
        !fixture.receipt().exists(),
        "an acked settlement must not leave its receipt behind"
    );
    let (mut again, mut out, _notes, _tx, _lease) = fixture.session();
    let frames = settle_draining(&mut again, &mut out, SettlementBoundary::Turn).await;
    let resent: Vec<String> = frames
        .iter()
        .filter(|f| f.method.as_deref() == Some(method::COMMIT_SETTLED))
        .map(|f| f.params.clone().unwrap().to_string())
        .collect();
    assert!(
        resent.is_empty(),
        "the acked settlement was reported all over again: {resent:?}"
    );
}

/// The other half of the derivation above: **nothing may be invented out of nowhere**.
///
/// When a new project is bound for the first time the agent repo on the hub is empty, the main
/// file line `rc land` builds is purely local, and `origin/*` holds no ref at all. A user opens
/// a session and closes it without running a turn — asked only whether "HEAD is unreachable
/// from origin", that scaffold commit becomes pending, so the supervisor pushes it and sends a
/// `commit.settled` pointing at it: a turn appears on the hub out of nowhere that is not this
/// session's output at all. The main line is not settlement's to report.
///
/// Remove the `--glob=refs/heads/main*` exclusion and this goes red at once: one extra
/// `commit.settled`.
#[cfg(unix)]
#[tokio::test]
async fn a_never_pushed_main_file_line_is_not_mistaken_for_a_pending_turn() {
    let fixture = SettlementFixture::new_local_main_line_only();
    let scaffold = fixture.head();
    assert!(
        fixture.tracking().is_empty(),
        "the premise needs an agent repo the hub has nothing in yet"
    );

    let (mut session, mut out, _notes, _tx, _lease) = fixture.session();
    let frames = settle_draining(&mut session, &mut out, SettlementBoundary::Turn).await;

    let settled: Vec<String> = frames
        .iter()
        .filter(|f| f.method.as_deref() == Some(method::COMMIT_SETTLED))
        .map(|f| f.params.clone().unwrap().to_string())
        .collect();
    assert!(
        settled.is_empty(),
        "the main file line was reported to the hub as a settled turn: {settled:?}"
    );
    assert_eq!(
        fixture.head(),
        scaffold,
        "the premise broke: this settlement was supposed to produce nothing"
    );
}

/// Regression (exit settlement blocked by the preceding delivery): the preceding
/// `commit.settled` is still Pending in the same epoch (the connection is alive, the outbound
/// queue is merely backed up). Yielding is right on a turn boundary; yielding on the exit
/// boundary throws away the final turn just drained: no commit, no push, `commit.settled` never
/// sent, while `Ended` goes out anyway.
///
/// The probe is `SessionNote::Bound` — the first observable action right after the delivery
/// gate (`announce_binding`). Change the `Pending` arm back to an unconditional `return` and
/// the exit side stops receiving it.
#[tokio::test(start_paused = true)]
async fn a_backlogged_delivery_does_not_swallow_the_final_settlement() {
    for (boundary, expect_bound) in [
        (SettlementBoundary::Turn, false),
        (SettlementBoundary::SessionExit, true),
    ] {
        let driver = AnyDriver::ClaudeCode(Box::new(
            crate::rc::harness::claude_code::ClaudeCodeDriver::test_driver(),
        ));
        let (mut session, _out, mut notes) = super::tests::harness_test_session_with_channels(
            driver,
            "claude-code",
            SessionStatus::Idle,
        );
        let lease = SettlementState {
            epoch: 7,
            agent_identity_v1: true,
            session_start_idempotency_v1: false,
        };
        let (_tx, rx) = tokio::sync::watch::channel(lease);
        session.settlement = rx;
        // The same epoch, Pending throughout: the kind a full `SETTLEMENT_DELIVERY_WAIT` still
        // does not resolve.
        let stuck = crate::protocol::ConnectionDelivery::new(
            lease.epoch,
            crate::protocol::ConnectionFeature::AgentIdentityV1,
        );
        session.pending_settlement = Some(PendingSettlement {
            sha: "cafebabe".into(),
            delivery: Some(stuck.clone()),
            receipt: None,
        });

        session.settle_and_push(boundary).await;

        assert_eq!(stuck.status(), crate::protocol::DeliveryStatus::Pending);
        let bound = matches!(notes.try_recv(), Ok(SessionNote::Bound { .. }));
        assert_eq!(
            bound,
            expect_bound,
            "boundary {} settlement went past the delivery gate: {bound}",
            if expect_bound { "exit" } else { "turn" }
        );
    }
}
