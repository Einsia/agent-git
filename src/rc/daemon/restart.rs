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
        self.idle_detachable_at(std::time::Instant::now())
    }

    /// [`Live::idle_detachable`] as of `now`.
    pub(super) fn idle_detachable_at(&self, now: std::time::Instant) -> bool {
        !self.shared_executor
            && self.started_process
            && self.info.status == SessionStatus::Idle
            && (self.settlement_unstartable
                || now.saturating_duration_since(self.status_since) >= IDLE_DETACH)
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
    /// The request in `frame` names a session: it is followed for another [`IDLE_DETACH`].
    pub(super) fn note_named(&mut self, frame: &Frame, now: std::time::Instant) {
        if let Some(live) = frame
            .params
            .as_ref()
            .and_then(|params| params["session_id"].as_str())
            .and_then(|id| self.sessions.get_mut(id))
        {
            live.last_named = now;
        }
    }

    /// End the writers nobody follows, as an upgrade ends them: idle past [`IDLE_DETACH`] with
    /// nothing queued, pending or awaiting an approval ([`Live::idle_detachable`]), and named by
    /// no request for as long. Each is recorded as paused, so the page says it was paused, not
    /// lost, and the next message resumes it. The daemon keeps no record of who reads a writer's
    /// events, so a request naming the session stands for a reader: a page left open with no
    /// request for that long sees the conversation paused and its next message resumes it.
    /// Without this, every conversation opened from the Hub keeps its runtime resident until the
    /// daemon exits.
    pub(super) fn reap_idle_writers(&mut self, now: std::time::Instant) {
        let idle: Vec<String> = self
            .sessions
            .iter()
            .filter(|(id, live)| {
                live.idle_detachable_at(now)
                    && now.saturating_duration_since(live.last_named) >= IDLE_DETACH
                    && live.rpc_lanes_idle_except(None)
                    // A turn recorded open has not been seen to end; stopping its writer would
                    // leave it to be closed later as interrupted.
                    && !self.roster.open_turns.contains_key(*id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        if idle.is_empty() {
            return;
        }
        let at = chrono::Utc::now().to_rfc3339();
        let mut recorded = false;
        for id in idle {
            let Some(live) = self.sessions.get_mut(&id) else {
                continue;
            };
            if live.tx.try_send(Command::ShutdownUnattended).is_err() {
                continue;
            }
            // The writer stays listed until its stop completes; a later pass must not stop it
            // again or record a second pause.
            live.last_named = now;
            crate::rc::diagnostics::event(
                "session.ended",
                serde_json::json!({
                    "session_id": crate::rc::diagnostics::id(&id),
                    "generation": live.generation,
                    "reason": "idle",
                }),
            );
            if self.roster.get(&id).is_some() {
                // The row now reads as paused to the lists that show it.
                self.list_changed(&id);
                self.roster
                    .paused
                    .insert(id, crate::protocol::Pause { at: at.clone() });
                recorded = true;
            }
        }
        if recorded && let Err(error) = self.roster.save() {
            eprintln!("agitd: the paused conversations could not be recorded: {error:#}");
        }
    }

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

    /// A writer nobody follows ends on its own, as an upgrade ends it: idle past [`IDLE_DETACH`]
    /// and named by no request for as long, it is told to stop once and recorded as paused, with
    /// its roster entry kept, so the next subscriber reads it as paused and its next message
    /// resumes it. One a request named within that time, one waiting for an approval, one with
    /// work queued, one whose RPC is in flight and one with a turn recorded open keep running.
    /// A daemon that never ended idle writers keeps every conversation opened from the Hub
    /// resident; one that went by status alone ends a conversation someone is still reading; one
    /// that skipped any of the other checks cuts off an approval, a queued message, a reply on
    /// its way or a turn; one that stopped a writer again on the next pass, after the supervisor
    /// took the first stop, records a second pause.
    #[test]
    fn an_idle_writer_nobody_names_is_paused_and_a_followed_or_busy_one_keeps_running() {
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    use crate::protocol::PermissionMode;
                    let now = std::time::Instant::now();
                    let long_ago = now
                        .checked_sub(IDLE_DETACH + std::time::Duration::from_secs(1))
                        .unwrap();
                    let mut roster = Roster::default();
                    let mut sessions = HashMap::new();
                    let mut commands = HashMap::new();
                    for (id, status) in [
                        ("agit-left", SessionStatus::Idle),
                        ("agit-named", SessionStatus::Idle),
                        ("agit-asking", SessionStatus::AwaitingApproval),
                        ("agit-queued", SessionStatus::Idle),
                        ("agit-replying", SessionStatus::Idle),
                        ("agit-turning", SessionStatus::Idle),
                    ] {
                        roster.sessions.insert(
                            id.into(),
                            super::super::tests::rpc_test_roster_entry(id, PermissionMode::Default),
                        );
                        let (tx, rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
                        commands.insert(id, rx);
                        let mut live =
                            super::super::tests::rpc_test_live(id, 1, tx, PermissionMode::Default);
                        live.info.status = status;
                        live.status_since = long_ago;
                        live.last_named = long_ago;
                        sessions.insert(id.to_string(), live);
                    }
                    roster.open_turns.insert(
                        "agit-turning".into(),
                        crate::rc::roster::OpenTurn {
                            turn_id: "turn-1".into(),
                            runtime: "codex".into(),
                            thread_id: "thread-agit-turning".into(),
                            cwd: "/w".into(),
                            writer: None,
                            stopped_by_person: false,
                        },
                    );
                    sessions["agit-queued"]
                        .tx
                        .try_send(Command::Shutdown)
                        .unwrap();
                    let _held = sessions["agit-replying"]
                        .rpc_gate
                        .clone()
                        .lock_owned()
                        .await;
                    let daemon = super::super::tests::rpc_test_daemon(sessions, roster);
                    let mut state = daemon.lock().await;
                    let named = Frame::request(
                        method::SESSION_SUBSCRIBE,
                        serde_json::json!({"session_id": "agit-named"}),
                    );
                    state.note_named(&named, now);

                    state.reap_idle_writers(now);
                    // The supervisor takes the stop at once; the writer stays listed until it ends.
                    assert!(matches!(
                        commands.get_mut("agit-left").unwrap().try_recv(),
                        Ok(Command::ShutdownUnattended)
                    ));
                    assert!(
                        !state.list_changes.take().is_empty(),
                        "the list showing the paused row hears of it"
                    );
                    state.reap_idle_writers(now + std::time::Duration::from_secs(1));
                    assert!(
                        state.list_changes.is_empty(),
                        "a writer not stopped again changes no list"
                    );
                    assert!(
                        commands.get_mut("agit-left").unwrap().try_recv().is_err(),
                        "a writer already told to stop is not told again"
                    );
                    for id in ["agit-named", "agit-asking", "agit-replying", "agit-turning"] {
                        assert!(
                            commands.get_mut(id).unwrap().try_recv().is_err(),
                            "{id} keeps running"
                        );
                    }
                    assert!(matches!(
                        commands.get_mut("agit-queued").unwrap().try_recv(),
                        Ok(Command::Shutdown)
                    ));
                    assert!(
                        commands.get_mut("agit-queued").unwrap().try_recv().is_err(),
                        "a writer with work queued keeps running"
                    );
                    let saved = Roster::load();
                    assert_eq!(saved.paused.keys().collect::<Vec<_>>(), ["agit-left"]);
                    assert!(
                        saved.sessions.contains_key("agit-left"),
                        "its next message resumes it from the roster"
                    );
                    assert_eq!(
                        saved.open_turns.keys().collect::<Vec<_>>(),
                        ["agit-turning"],
                        "an open turn is neither closed nor added by the reaper"
                    );

                    // Its stop completed: the next subscriber hears it was paused.
                    state.sessions.remove("agit-left");
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
                    assert!(data["paused"]["at"].is_string(), "{data}");
                })
        });
    }

    /// An upgrade ends a conversation on a runtime only the daemon runs once it has sat idle past
    /// [`IDLE_DETACH`], and the conversation then tells a subscriber it was paused for the update,
    /// with nothing else. One idle for less, or waiting for an approval, still holds the upgrade.
    /// An upgrade that waits on every idle conversation is held for ever by one forgotten
    /// conversation; one that ends a young or busy one cuts someone off; one that ends it silently
    /// leaves the person thinking it was lost. Its session hears that the daemon is going before
    /// the stop itself: a stop alone reads as the conversation's own end, which drops the messages
    /// waiting for its next turn instead of keeping them for the session that continues it.
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
                    let told: Vec<_> =
                        std::iter::from_fn(|| receivers[0].try_recv().ok()).collect();
                    assert!(
                        matches!(
                            told.as_slice(),
                            [Command::DaemonExiting, Command::ShutdownUnattended]
                        ),
                        "the session hears the daemon is going, then the stop"
                    );
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
