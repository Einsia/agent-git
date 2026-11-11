use super::*;
use crate::rc::admission::{Admission, Frozen};

pub(super) struct SafeStopRequest {
    pub instance_id: String,
    pub build_id: String,
    pub replacing_with: Option<String>,
    pub deadline: std::time::Instant,
    pub reply: std::sync::mpsc::SyncSender<control::Reply>,
    pub written: tokio::sync::oneshot::Receiver<()>,
}

#[derive(Debug)]
pub(super) enum RestartRefusal {
    InstanceChanged,
    Busy(Vec<String>),
}

impl From<RestartRefusal> for control::Reply {
    fn from(value: RestartRefusal) -> Self {
        match value {
            RestartRefusal::InstanceChanged => Self::InstanceChanged,
            RestartRefusal::Busy(blockers) => Self::Busy { blockers },
        }
    }
}

impl Live {
    /// A conversation on a runtime only the daemon runs that has sat idle past [`IDLE_DETACH`]
    /// with nothing queued or pending: an upgrade may end it, and it continues with the next
    /// message. One whose saving step couldn't start need not wait that long: only the restart
    /// saves its turns, so it may go as soon as its turn is over.
    pub(super) fn idle_detachable(&self) -> bool {
        !self.shared_executor
            && self.started_process
            && self.info.status == SessionStatus::Idle
            && (self.settlement_unstartable || self.status_since.elapsed() >= IDLE_DETACH)
            && self.tx.capacity() == COMMAND_QUEUE_CAPACITY
            && !self.rpc_guard_sensitive
            && self.pending_mode.is_none()
            && self.confirmed_turn_guards.is_empty()
            && self.inflight_turn_guard.is_none()
            && self.approval_requests.is_empty()
            && !self.ended
    }
}

impl Daemon {
    pub(super) fn prepare_safe_stop(
        &self,
        request: &SafeStopRequest,
        admission: &Admission,
        rpc_active: bool,
        peers_active: impl FnOnce() -> bool,
    ) -> Result<Frozen, RestartRefusal> {
        if request.instance_id != self.identity.instance_id
            || request.build_id != self.identity.build_id
        {
            return Err(RestartRefusal::InstanceChanged);
        }
        if std::time::Instant::now() >= request.deadline {
            return Err(RestartRefusal::Busy(vec![
                "safe restart request expired before admission".into(),
            ]));
        }
        let frozen = admission
            .freeze()
            .map_err(|reason| RestartRefusal::Busy(vec![reason]))?;
        let mut blockers = Vec::new();
        // Each blocking session is named, by id only, so whoever waits for the restart can tell
        // which conversation to close.
        let held: Vec<&str> = self
            .sessions
            .iter()
            .filter(|(_, live)| !live.idle_detachable())
            .filter(|(_, live)| {
                !live.shared_executor
                    || live.info.status != SessionStatus::Idle
                    || live.runtime_thread_id.is_none()
                    || live.tx.capacity() != COMMAND_QUEUE_CAPACITY
                    || live.rpc_guard_sensitive
                    || live.pending_mode.is_some()
                    || !live.confirmed_turn_guards.is_empty()
                    || live.inflight_turn_guard.is_some()
                    || live.ended
            })
            .map(|(id, _)| id.as_str())
            .collect();
        if !held.is_empty() {
            blockers.push(format!(
                "supervised work cannot be detached safely: {}",
                held.join(", ")
            ));
        }
        if !self.opening_sessions.is_empty() {
            blockers.push("session launches are still reserved".into());
        }
        if self
            .roster
            .starts
            .values()
            .any(|intent| matches!(intent.state, roster::StartState::Pending { .. }))
        {
            blockers.push("a session start has an unresolved durable receipt".into());
        }
        if !self.terminals.is_empty()
            || self
                .terminal_delivery_blockers
                .load(std::sync::atomic::Ordering::Acquire)
                != 0
        {
            blockers.push("terminal processes or their final output are still active".into());
        }
        if rpc_active {
            blockers.push("session RPC workers are still active".into());
        }
        if peers_active() {
            blockers.push("peer connections are still owned by this controller".into());
        }
        if !blockers.is_empty() {
            return Err(RestartRefusal::Busy(blockers));
        }
        Ok(frozen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An idle conversation normally holds an upgrade for [`IDLE_DETACH`], so a person who
    /// steps away comes back to it as they left it. One whose saving step couldn't start holds
    /// it only until its turn is over: only the restart saves its turns. A running one still
    /// holds it, so a turn in progress is never cut off.
    #[tokio::test]
    async fn a_session_that_cannot_save_holds_an_upgrade_only_until_its_turn_ends() {
        let (tx, _commands) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let mut live = super::super::tests::rpc_test_live(
            "busy",
            1,
            tx,
            crate::protocol::PermissionMode::Default,
        );
        live.shared_executor = false;
        live.started_process = true;
        live.info.status = SessionStatus::Idle;
        live.status_since = std::time::Instant::now();
        assert!(
            !live.idle_detachable(),
            "a fresh idle session holds the upgrade"
        );
        live.settlement_unstartable = true;
        assert!(live.idle_detachable());
        live.info.status = SessionStatus::Running;
        assert!(!live.idle_detachable(), "a running turn is never cut off");
    }

    #[tokio::test]
    async fn safe_stop_preserves_live_sessions_and_launch_reservations() {
        let (tx, mut commands) = mpsc::channel(1);
        let mut live = super::super::tests::rpc_test_live(
            "active",
            1,
            tx,
            crate::protocol::PermissionMode::Default,
        );
        live.info.status = SessionStatus::AwaitingApproval;
        let daemon = super::super::tests::rpc_test_daemon(
            HashMap::from([("active".into(), live)]),
            Roster::default(),
        );
        let mut state = daemon.lock().await;
        let (reply, _received) = std::sync::mpsc::sync_channel(1);
        let (_written, written) = tokio::sync::oneshot::channel();
        let mut request = SafeStopRequest {
            instance_id: state.identity.instance_id.clone(),
            build_id: state.identity.build_id.clone(),
            replacing_with: None,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
            reply,
            written,
        };
        let admission = Admission::default();
        assert!(matches!(
            state.prepare_safe_stop(&request, &admission, false, || false),
            Err(RestartRefusal::Busy(_))
        ));
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(
            admission.enter().is_some(),
            "a deferred restart leaves admission open"
        );
        state.sessions.get_mut("active").unwrap().info.status = SessionStatus::Idle;
        // The refusal names the session that holds it, so the person can tell which to close.
        match state.prepare_safe_stop(&request, &admission, false, || false) {
            Err(RestartRefusal::Busy(blockers)) => assert!(
                blockers.iter().any(|blocker| blocker.ends_with(": active")),
                "{blockers:?}"
            ),
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("an idle unshared session let the restart through"),
        }
        state.sessions.clear();
        state.opening_sessions.insert(
            "opening".into(),
            LaunchReservation {
                generation: 2,
                runtime: "codex".into(),
                native_source: None,
                native_id: None,
            },
        );
        assert!(matches!(
            state.prepare_safe_stop(&request, &admission, false, || false),
            Err(RestartRefusal::Busy(_))
        ));
        state.opening_sessions.clear();
        request.instance_id = "another-instance".into();
        assert!(matches!(
            state.prepare_safe_stop(&request, &admission, false, || false),
            Err(RestartRefusal::InstanceChanged)
        ));
        request.instance_id = state.identity.instance_id.clone();
        let frozen = state
            .prepare_safe_stop(&request, &admission, false, || false)
            .unwrap_or_else(|reply| panic!("{reply:?}"));
        assert!(admission.enter().is_none());
        drop(frozen);
        request.deadline = std::time::Instant::now();
        assert!(matches!(
            state.prepare_safe_stop(&request, &admission, false, || false),
            Err(RestartRefusal::Busy(_))
        ));
        assert!(admission.enter().is_some());
    }

    #[tokio::test]
    async fn safe_stop_detaches_only_idle_shared_executors_with_empty_command_queues() {
        let (tx, mut commands) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let mut live = super::super::tests::rpc_test_live(
            "shared",
            1,
            tx,
            crate::protocol::PermissionMode::Default,
        );
        live.info.status = SessionStatus::Idle;
        live.shared_executor = true;
        live.runtime_thread_id = Some("native-thread".into());
        let daemon = super::super::tests::rpc_test_daemon(
            HashMap::from([("shared".into(), live)]),
            Roster::default(),
        );
        let mut state = daemon.lock().await;
        let (reply, _received) = std::sync::mpsc::sync_channel(1);
        let (_written, written) = tokio::sync::oneshot::channel();
        let request = SafeStopRequest {
            instance_id: state.identity.instance_id.clone(),
            build_id: state.identity.build_id.clone(),
            replacing_with: None,
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
            reply,
            written,
        };
        let admission = Admission::default();
        let frozen = state
            .prepare_safe_stop(&request, &admission, false, || false)
            .expect("an idle shared executor can disconnect without stopping its native server");
        assert!(admission.enter().is_none());
        drop(frozen);
        state.sessions.get_mut("shared").unwrap().info.status = SessionStatus::Running;
        assert!(matches!(
            state.prepare_safe_stop(&request, &admission, false, || false),
            Err(RestartRefusal::Busy(_))
        ));
        state.sessions.get_mut("shared").unwrap().info.status = SessionStatus::Idle;
        state
            .sessions
            .get_mut("shared")
            .unwrap()
            .tx
            .try_send(Command::Shutdown)
            .unwrap();
        assert!(matches!(
            state.prepare_safe_stop(&request, &admission, false, || false),
            Err(RestartRefusal::Busy(_))
        ));
        assert!(matches!(commands.try_recv(), Ok(Command::Shutdown)));
        state.sessions.get_mut("shared").unwrap().shared_executor = false;
        assert!(matches!(
            state.prepare_safe_stop(&request, &admission, false, || false),
            Err(RestartRefusal::Busy(_))
        ));
    }

    /// An upgrade ends a conversation on a runtime only the daemon runs once it has sat idle past
    /// [`IDLE_DETACH`], and the conversation then tells a subscriber it was paused for the update,
    /// with nothing else. One idle for less, or waiting for an approval, still holds the upgrade.
    /// An upgrade that waits on every idle conversation is held for ever by one forgotten
    /// conversation; one that ends a young or busy one cuts someone off; one that ends it silently
    /// leaves the person thinking it was lost.
    #[test]
    fn an_upgrade_ends_only_long_idle_conversations_and_says_they_were_paused() {
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    use crate::protocol::PermissionMode;
                    let long_ago = std::time::Instant::now()
                        .checked_sub(IDLE_DETACH + std::time::Duration::from_secs(1))
                        .unwrap();
                    let mut roster = Roster::default();
                    let mut sessions = HashMap::new();
                    let mut receivers = vec![];
                    for (id, status, since) in [
                        ("agit-left", SessionStatus::Idle, long_ago),
                        (
                            "agit-reading",
                            SessionStatus::Idle,
                            std::time::Instant::now(),
                        ),
                        ("agit-asking", SessionStatus::AwaitingApproval, long_ago),
                    ] {
                        roster.sessions.insert(
                            id.into(),
                            super::super::tests::rpc_test_roster_entry(id, PermissionMode::Default),
                        );
                        let (tx, rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
                        receivers.push(rx);
                        let mut live =
                            super::super::tests::rpc_test_live(id, 1, tx, PermissionMode::Default);
                        live.info.status = status;
                        live.status_since = since;
                        sessions.insert(id.to_string(), live);
                    }
                    let daemon = super::super::tests::rpc_test_daemon(sessions, roster);
                    let mut state = daemon.lock().await;
                    let (reply, _received) = std::sync::mpsc::sync_channel(1);
                    let (_written, written) = tokio::sync::oneshot::channel();
                    let request = SafeStopRequest {
                        instance_id: state.identity.instance_id.clone(),
                        build_id: state.identity.build_id.clone(),
                        replacing_with: None,
                        deadline: std::time::Instant::now() + std::time::Duration::from_secs(60),
                        reply,
                        written,
                    };
                    let admission = Admission::default();
                    match state.prepare_safe_stop(&request, &admission, false, || false) {
                        Err(RestartRefusal::Busy(blockers)) => {
                            let held = blockers.join("; ");
                            assert!(held.contains("agit-reading"), "{held}");
                            assert!(held.contains("agit-asking"), "{held}");
                            assert!(!held.contains("agit-left"), "{held}");
                        }
                        Err(other) => panic!("{other:?}"),
                        Ok(_) => {
                            panic!("an idle conversation under the bound let the upgrade through")
                        }
                    }
                    state.sessions.retain(|id, _| id == "agit-left");
                    let frozen = state
                        .prepare_safe_stop(&request, &admission, false, || false)
                        .unwrap_or_else(|reply| panic!("{reply:?}"));
                    drop(frozen);
                    state.stop_cause = StopCause::Upgrade;
                    state.shutdown().await;
                    let saved = Roster::load();
                    assert_eq!(saved.paused.keys().collect::<Vec<_>>(), ["agit-left"]);
                    assert!(saved.interrupted.is_empty(), "nothing was cut");
                    assert_eq!(saved.sessions["agit-left"].thread_id, "thread-agit-left");

                    let (frames, _) = mpsc::channel(1);
                    let mut subscribe = Frame::request(
                        method::SESSION_SUBSCRIBE,
                        serde_json::json!({"session_id": "agit-left"}),
                    );
                    subscribe.caller = Some(super::super::tests::claim("viewer", "ws-a"));
                    let data = state
                        .dispatch(&subscribe, &frames)
                        .await
                        .unwrap_err()
                        .data
                        .unwrap();
                    assert_eq!(
                        data["paused"]
                            .as_object()
                            .unwrap()
                            .keys()
                            .map(String::as_str)
                            .collect::<Vec<_>>(),
                        ["at"]
                    );
                    assert!(data.get("interrupted").is_none());
                })
        });
    }
}
