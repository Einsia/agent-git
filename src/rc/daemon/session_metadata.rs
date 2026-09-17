use super::*;

pub(super) fn is_session_metadata(method_name: &str) -> bool {
    matches!(
        method_name,
        method::SESSION_COMMANDS | method::SESSION_MODEL
    )
}

pub(super) struct PreparedSessionMetadata {
    session_id: String,
    generation: u64,
    tx: mpsc::Sender<Command>,
    command: Command,
    reply: SessionReceipt<serde_json::Value>,
}

impl Daemon {
    pub(super) fn prepare_session_metadata(
        &self,
        frame: &Frame,
    ) -> Result<PreparedSessionMetadata, RpcError> {
        let caller = caller_scope(frame)?;
        require_role(&caller, frame.method())?;
        let session_id = queued_session_id(frame)?;
        let driving = self.session_channel(&session_id, &caller, Need::Drive)?;
        let live = &self.sessions[&session_id];
        let (ticket, receipt) = crate::rc::ticket::ticket_authorized(frame.authority.clone());
        let command = match frame.method() {
            method::SESSION_COMMANDS => Command::Runtime {
                name: "commands".into(),
                arguments: serde_json::json!({}),
                reply: ticket,
            },
            method::SESSION_MODEL => Command::Model {
                model: None,
                reply: ticket,
            },
            _ => return Err(RpcError::new(ErrorCode::Internal, "not session metadata")),
        };
        Ok(PreparedSessionMetadata {
            session_id,
            generation: live.generation,
            tx: driving.tx,
            command,
            reply: SessionReceipt(receipt),
        })
    }
}

impl PreparedSessionMetadata {
    // Read-only metadata uses the harness queue but never owns or projects a writer guard.
    pub(super) async fn execute(
        self,
        daemon: Arc<Mutex<Daemon>>,
        stop: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<serde_json::Value, RpcError> {
        let Self {
            session_id,
            generation,
            tx,
            command,
            mut reply,
        } = self;
        enqueue_within(&tx, command, SESSION_REPLY_TIMEOUT, stop).await?;
        let result = match reply_within_state(reply.get_mut(), stop).await {
            ReplyWait::Done(result) => result,
            ReplyWait::InFlight(error) => Err(error),
        };
        let state = daemon.lock().await;
        if !state
            .sessions
            .get(&session_id)
            .is_some_and(|live| live.generation == generation && !live.ended)
        {
            return Err(no_such_session(&session_id));
        }
        result
    }
}
