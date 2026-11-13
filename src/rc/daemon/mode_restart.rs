//! A permission switch the running process cannot take live.
//!
//! Claude Code started with permission checks refuses `set_permission_mode bypassPermissions`
//! (`bypass_not_launched`): only a process launched with `--dangerously-skip-permissions` switches
//! into Bypass. So when the owner switches such a conversation to Bypass between turns, the daemon
//! stops its process and brings it back in Bypass through the resume path, the same launch a
//! resume naming that mode makes. While a turn runs, or a terminal is taking the conversation over,
//! the switch is refused rather than cutting either short.
//!
//! Instructions for the conversation that arrive while it restarts wait for its new process
//! instead of finding it gone: a page that is told "busy" or "no such session" sends again, and
//! that is how one message turns into several. A person's stop during the restart wins: the
//! conversation stays stopped in its old mode.

use super::*;

/// A restart under way, as instructions and stops for its conversation find it.
pub(super) struct ModeRestart {
    /// Set once the conversation runs again or the restart gives up; closed if it was dropped unrun.
    done: tokio::sync::watch::Receiver<bool>,
    /// Set by a person's `session.stop` or `session.close` meanwhile: nothing is launched again.
    stop: tokio::sync::watch::Sender<bool>,
}

impl ModeRestart {
    /// Whether the restart can still bring the conversation back.
    pub(super) fn under_way(&self) -> bool {
        self.done.has_changed().is_ok()
    }

    /// Ask the restart to leave the conversation stopped.
    pub(super) fn stop(&self) {
        let _ = self.stop.send(true);
    }

    /// A restart under way until `done` is set, and the receiver of its stop.
    #[cfg(test)]
    pub(super) fn waiting_on(
        done: tokio::sync::watch::Receiver<bool>,
    ) -> (Self, tokio::sync::watch::Receiver<bool>) {
        let (stop, stopped) = tokio::sync::watch::channel(false);
        (Self { done, stop }, stopped)
    }
}

/// What the restart operation holds: the resume that brings the conversation back, the sender
/// that releases what waits on it, and the receiver of a person's stop.
pub(super) type PreparedRestart = (
    SessionResume,
    tokio::sync::watch::Sender<bool>,
    tokio::sync::watch::Receiver<bool>,
);

/// How long the old process gets to end, the settling of its last turn included.
const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(60);
const STOP_POLL: std::time::Duration = std::time::Duration::from_millis(50);
/// How long an instruction waits for a restart: the old process ending, then the new one opening.
const HOLD_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

impl Daemon {
    /// The restart that switches `session_id` to `p.mode`, refused unless the conversation sits
    /// between turns with nothing else pending. Instructions for it wait from here on.
    pub(super) fn prepare_mode_restart(
        &mut self,
        p: &crate::protocol::SessionSetPermissionMode,
        lane: SessionRpcLane,
    ) -> Result<PreparedRestart, RpcError> {
        let live = self
            .sessions
            .get(&p.session_id)
            .ok_or_else(|| no_such_session(&p.session_id))?;
        let handing_over = live
            .runtime_thread_id
            .as_deref()
            .is_some_and(|thread| self.leases.releasing(thread));
        if handing_over {
            return Err(restart_refused(
                "Turning on Bypass restarts the agent, and a terminal is taking this conversation over. Switch again once it has.",
            ));
        }
        let busy = live.info.status != SessionStatus::Idle
            || live.tx.capacity() != COMMAND_QUEUE_CAPACITY
            || !live.approval_requests.is_empty()
            || !live.handed_approvals.is_empty()
            || live.pending_mode.is_some()
            || !live.confirmed_turn_guards.is_empty()
            || live.inflight_turn_guard.is_some()
            || !live.restart_guard_attempts.is_empty()
            || !live.rpc_lanes_idle_except(Some(lane));
        if busy {
            return Err(restart_refused(
                "Turning on Bypass restarts the agent, and a turn is running. Switch again when this turn ends.",
            ));
        }
        // A conversation with an unconfirmed permission change comes back only in Plan, so the
        // restart could stop it and not bring it back in Bypass.
        if self
            .roster
            .sessions
            .get(&p.session_id)
            .is_some_and(|entry| !entry.guard_attempts.is_empty())
        {
            return Err(restart_refused(
                "Turning on Bypass restarts the agent, and its last permission change is still being confirmed. Switch again in a moment.",
            ));
        }
        let resume = SessionResume {
            workspace_id: live.info.workspace_id.clone(),
            session_id: p.session_id.clone(),
            project_id: None,
            prompt: None,
            by: p.by.clone(),
            agent: None,
            expected_agent_id: None,
            branch: None,
            attach_only: false,
            permission_mode: Some(p.mode),
        };
        let (done, waiting) = tokio::sync::watch::channel(false);
        let (stop, stopped) = tokio::sync::watch::channel(false);
        self.mode_restarts.insert(
            p.session_id.clone(),
            ModeRestart {
                done: waiting,
                stop,
            },
        );
        Ok((resume, done, stopped))
    }

    /// The restart an instruction for `session_id` has to wait for, if one is under way. One whose
    /// restart was dropped unrun is forgotten here.
    pub(super) fn mode_restart_of(
        &mut self,
        session_id: &str,
    ) -> Option<tokio::sync::watch::Receiver<bool>> {
        let restart = self.mode_restarts.get(session_id)?;
        if !restart.under_way() {
            self.mode_restarts.remove(session_id);
            return None;
        }
        Some(restart.done.clone())
    }

    /// A restart of `session_id` still under way, for a stop to cancel.
    pub(super) fn mode_restart_to_stop(&mut self, session_id: &str) -> Option<&ModeRestart> {
        self.mode_restart_of(session_id)?;
        self.mode_restarts.get(session_id)
    }
}

fn restart_refused(message: &str) -> RpcError {
    classified(
        RpcError::new(ErrorCode::SessionBusy, message),
        false,
        "turn_running",
    )
}

/// A person stopped the conversation while it restarted: it stays stopped, in its old mode unless
/// its new process was already up.
fn stopped_as_asked(restarted: bool) -> RpcError {
    let message = if restarted {
        "the conversation was stopped as you asked, just after it restarted in Bypass"
    } else {
        "the conversation was stopped as you asked while it restarted; its mode is unchanged"
    };
    classified(
        RpcError::new(ErrorCode::RuntimeUnavailable, message),
        false,
        "stopped",
    )
}

/// A terminal opened the conversation while its process was stopped, and holds it now.
fn taken_over() -> RpcError {
    classified(
        RpcError::new(
            ErrorCode::RuntimeUnavailable,
            "the conversation stopped to turn on Bypass, and a terminal on this computer took it over meanwhile; its mode is unchanged",
        ),
        false,
        "terminal_took_over",
    )
}

/// Whether a resume was refused because another writer holds the conversation: a terminal that
/// opened it, or one that wrote it moments ago.
fn writer_refusal(error: &RpcError) -> bool {
    let data = error.data.as_ref();
    data.is_some_and(|data| data.get("lease").is_some())
        || data
            .and_then(|data| data.get("reason"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|reason| {
                matches!(
                    reason,
                    "native_writer_active"
                        | "native_control_reconnecting"
                        | "native_recent_activity"
                )
            })
}

/// The conversation stopped for the switch but did not come back: it stays stopped in its old
/// mode, and the next message resumes it.
fn not_restarted(reason: &str) -> RpcError {
    classified(
        RpcError::new(
            ErrorCode::RuntimeUnavailable,
            format!(
                "the conversation stopped to turn on Bypass but did not start again ({reason}); its mode is unchanged, and sending a message resumes it"
            ),
        ),
        false,
        "restart_failed",
    )
}

/// Stops generation `generation` of `session_id` and opens the conversation again through
/// `resume`, giving the switch's reply. The caller has already released the old generation's
/// instruction gate, so its row can go once its process ends.
#[allow(clippy::too_many_arguments)]
pub(super) async fn restart_in_mode(
    daemon: Arc<Mutex<Daemon>>,
    session_id: &str,
    generation: u64,
    mode: crate::protocol::PermissionMode,
    resume: SessionResume,
    caller: crate::protocol::CallerClaim,
    authority: crate::rc::authority::Guard,
    mut stopped: tokio::sync::watch::Receiver<bool>,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<serde_json::Value, RpcError> {
    let by = resume.by.clone();
    let deadline = tokio::time::Instant::now() + STOP_WAIT;
    loop {
        let gone = !daemon
            .lock()
            .await
            .sessions
            .get(session_id)
            .is_some_and(|live| live.generation == generation);
        if gone {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(not_restarted("its process did not stop in time"));
        }
        tokio::select! {
            _ = stop.changed() => return Err(not_restarted("the daemon is stopping")),
            Ok(_) = stopped.wait_for(|stopped| *stopped) => return Err(stopped_as_asked(false)),
            _ = tokio::time::sleep(STOP_POLL) => {}
        }
    }
    // The stop and the resume are judged under one lock: a stop after this cancels the launch.
    let mut opening = {
        let mut state = daemon.lock().await;
        if *stopped.borrow() {
            return Err(stopped_as_asked(false));
        }
        // The switch's own admission still holds, or nothing launches.
        authority
            .check()
            .map_err(|error| not_restarted(&error.message))?;
        let frames = state
            .session_frames
            .clone()
            .ok_or_else(|| not_restarted("the daemon is not serving sessions"))?;
        state
            .prepare_resume_session(resume, &caller, &frames, &authority)
            .map_err(|error| {
                if writer_refusal(&error) {
                    taken_over()
                } else {
                    not_restarted(&error.message)
                }
            })?
    };
    // The launch is checked against the same admission when it executes, as a resume's is.
    if let SessionOpening::Launch(spawn, _) = &mut opening {
        spawn.authority = authority.clone();
    }
    // Something else brought it back first, in the mode it had: nothing was switched.
    if let SessionOpening::Ready(_) = &opening {
        return Err(not_restarted(
            "it was resumed meanwhile in its old mode; switch again",
        ));
    }
    // The launch stops where it is for the daemon's stop or a person's, and kills what it began.
    let (cancel, cancelled) = tokio::sync::watch::channel(false);
    let mut daemon_stop = stop.clone();
    let mut person_stop = stopped.clone();
    let forward = tokio::spawn(async move {
        tokio::select! {
            Ok(_) = daemon_stop.wait_for(|stopping| *stopping) => {}
            Ok(_) = person_stop.wait_for(|stopped| *stopped) => {}
            else => return,
        }
        let _ = cancel.send(true);
    });
    let opened = opening.run(daemon.clone(), cancelled).await;
    forward.abort();
    if *stopped.borrow() {
        if opened.is_ok() {
            // It ran before the stop reached the launch: stop it now, as asked.
            if let Some(live) = daemon.lock().await.sessions.get(session_id) {
                let _ = live.tx.try_send(Command::Shutdown);
            }
        }
        return Err(stopped_as_asked(opened.is_ok()));
    }
    opened.map_err(|error| not_restarted(&error.message))?;
    // Viewers learn the new mode as they do from a live switch.
    let state = daemon.lock().await;
    if let (Some(live), Some(frames)) =
        (state.sessions.get(session_id), state.session_frames.clone())
    {
        let mut frame = Frame::notification(
            method::SESSION_PERMISSION_MODE,
            crate::protocol::SessionPermissionMode {
                session_id: session_id.to_owned(),
                mode: Some(mode),
                native_default: false,
                applied: crate::protocol::PermissionApply::Immediate,
                by,
            },
        );
        tag_session_frame(&mut frame, session_id, live.generation);
        let _ = frames.try_send(frame);
    }
    drop(state);
    Ok(
        serde_json::to_value(crate::protocol::SessionSetPermissionModeResult {
            mode,
            applied: crate::protocol::PermissionApply::Immediate,
        })
        .unwrap(),
    )
}

/// Waits until `restart` has finished, or been dropped unrun. The error is the answer for an
/// instruction still waiting after [`HOLD_WAIT`], or when the daemon stops.
async fn wait_for_restart(
    restart: &mut tokio::sync::watch::Receiver<bool>,
    deadline: tokio::time::Instant,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<(), RpcError> {
    // A sender dropped unrun ends the wait as surely as one that finished.
    let waited = tokio::select! {
        _ = stop.changed() => None,
        done = tokio::time::timeout_at(deadline, restart.wait_for(|done| *done)) => {
            Some(done.map(|_| ()))
        }
    };
    match waited {
        None => Err(RpcError::new(
            ErrorCode::SessionBusy,
            "the daemon is stopping; nothing was sent",
        )),
        Some(Err(_)) => Err(RpcError::new(
            ErrorCode::SessionBusy,
            "the conversation is still restarting to change its mode; nothing was sent",
        )
        .with_hint("retry once it shows as running again")),
        Some(Ok(())) => Ok(()),
    }
}

/// Serves a queued instruction for a conversation that is restarting once it runs again, as the
/// pump would have served it then. One still waiting after [`HOLD_WAIT`] is answered busy.
pub(super) async fn serve_after_restart(
    daemon: Arc<Mutex<Daemon>>,
    outbound: crate::rc::outbound::OutboundTx,
    id: crate::protocol::RequestId,
    frame: Frame,
    connection_epoch: u64,
    mut restart: tokio::sync::watch::Receiver<bool>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let deadline = tokio::time::Instant::now() + HOLD_WAIT;
    let session_id = queued_session_id(&frame).unwrap_or_default();
    let prepared = loop {
        if let Err(error) = wait_for_restart(&mut restart, deadline, &mut stop).await {
            let _ = outbound.send(Frame::error_response(id, instruction_not_sent(error)));
            return;
        }
        let mut state = daemon.lock().await;
        if !connection_epoch_is_current(&state.settlement, connection_epoch) {
            return;
        }
        // Another restart may have begun meanwhile; wait for that one too.
        match state.mode_restart_of(&session_id) {
            Some(next) => restart = next,
            None => break state.prepare_session_rpc_or_wait(&frame),
        }
    };
    match prepared {
        Ok(SessionRpcPreparation::Ready(prepared)) => {
            (*prepared).serve(daemon, outbound, id, stop).await;
        }
        Ok(SessionRpcPreparation::AwaitingDurableGuardRow(lease)) => {
            lease
                .serve_when_bound(SessionRpcBoundWait {
                    daemon,
                    outbound,
                    id,
                    frame,
                    connection_epoch,
                    stop,
                })
                .await;
        }
        Err(error) => {
            let _ = outbound.send(Frame::error_response(id, error));
        }
    }
}

/// Serves a resume of a conversation that is restarting once it runs again: by then it is
/// supervised, and the resume answers with it as it runs.
#[allow(clippy::too_many_arguments)]
pub(super) async fn open_after_restart(
    daemon: Arc<Mutex<Daemon>>,
    outbound: crate::rc::outbound::OutboundTx,
    frames: mpsc::Sender<Frame>,
    id: crate::protocol::RequestId,
    frame: Frame,
    connection_epoch: u64,
    mut restart: tokio::sync::watch::Receiver<bool>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let deadline = tokio::time::Instant::now() + HOLD_WAIT;
    let session_id = frame
        .params_as::<SessionResume>()
        .map(|p| p.session_id)
        .unwrap_or_default();
    let prepared = loop {
        if let Err(error) = wait_for_restart(&mut restart, deadline, &mut stop).await {
            let _ = outbound.send(Frame::error_response(id, error));
            return;
        }
        let mut state = daemon.lock().await;
        if !connection_epoch_is_current(&state.settlement, connection_epoch) {
            return;
        }
        match state.mode_restart_of(&session_id) {
            Some(next) => restart = next,
            None => break state.prepare_opening(&frame, &frames),
        }
    };
    match prepared {
        Ok(opening) => opening.serve(daemon, outbound, id, stop).await,
        Err(error) => {
            let _ = outbound.send(Frame::error_response(id, error));
        }
    }
}
