use super::*;
use crate::rc::admission::{Admission, Frozen};

pub(super) struct SafeStopRequest {
    pub instance_id: String,
    pub build_id: String,
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
        if !self.sessions.is_empty() {
            blockers.push("supervised sessions are still alive".into());
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
        assert!(matches!(
            state.prepare_safe_stop(&request, &admission, false, || false),
            Err(RestartRefusal::Busy(_))
        ));
        state.sessions.clear();
        state.opening_sessions.insert(
            "opening".into(),
            LaunchReservation {
                generation: 2,
                runtime: "codex".into(),
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
}
