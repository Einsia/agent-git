use super::*;

pub(super) struct TurnAttachments {
    pub(super) files: serde_json::Value,
    pub(super) caller: crate::protocol::CallerClaim,
    pub(super) authority: crate::rc::authority::Guard,
    pub(super) epoch: u64,
}

impl TurnAttachments {
    pub(super) async fn stage_and_enqueue(
        self,
        daemon: &Mutex<Daemon>,
        session: &str,
        generation: u64,
        tx: &mpsc::Sender<Command>,
        mut command: Command,
        stop: &mut tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), RpcError> {
        let permit = reserve_within(tx, SESSION_REPLY_TIMEOUT, stop).await?;
        let input = command.client_msg_id().unwrap_or_default().to_owned();
        let message = match &mut command {
            Command::Turn { message, .. }
            | Command::Steer { message, .. }
            | Command::StopAndSend { message, .. } => message,
            _ => unreachable!("only session input carries attachments"),
        };
        let caption = std::mem::take(message);
        let target = session.to_owned();
        let authority = self.authority.clone();
        let owner = self.caller.is_owner();
        let (caption, reference) = tokio::task::spawn_blocking(move || {
            authority.admit_turn_files(owner, &target, || ())?;
            let root = crate::rc::rc_dir().map_err(|_| {
                RpcError::new(ErrorCode::Internal, "attachment directory unavailable")
            })?;
            if self.files.is_null() {
                Ok::<_, RpcError>((caption, None))
            } else {
                let custody = authority.turn_files_owner(owner, &target)?;
                let staged = crate::rc::input_attachments::stage(
                    &root.join("task-attachments"),
                    custody,
                    &target,
                    &input,
                    &self.files,
                    &caption,
                )
                .map_err(|error| {
                    let message = error
                        .downcast_ref::<crate::rc::input_attachments::Unavailable>()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "could not verify or store turn attachments".to_owned());
                    RpcError::new(ErrorCode::MalformedFrame, message)
                })?;
                Ok((staged.message, Some(staged.reference)))
            }
        })
        .await
        .map_err(|_| RpcError::new(ErrorCode::Internal, "attachment worker stopped"))??;
        *message = caption;
        if let Some(reference) = reference {
            let references = vec![reference];
            match &mut command {
                Command::Turn { reply, .. } => reply.set_attachments(references),
                Command::Steer { reply, .. } | Command::StopAndSend { reply, .. } => {
                    reply.set_attachments(references)
                }
                _ => unreachable!("only session input carries attachments"),
            }
        }

        // A reserved slot makes the final send nonblocking. Neither disk I/O nor
        // backpressure can hold the daemon lock or extend a grant across revocation.
        let state = daemon.lock().await;
        if *stop.borrow() || stop.has_changed().is_err() {
            return Err(RpcError::new(
                ErrorCode::SessionBusy,
                "this daemon is stopping",
            ));
        }
        if !connection_epoch_is_current(&state.settlement, self.epoch) {
            return Err(RpcError::new(
                ErrorCode::Forbidden,
                "connection changed during attachment staging",
            ));
        }
        let live = state
            .sessions
            .get(session)
            .ok_or_else(|| no_such_session(session))?;
        if live.ended || live.generation != generation {
            return Err(no_such_session(session));
        }
        state.session_channel(session, &self.caller, Need::Drive)?;
        if let Some(error) = state.lease_refusal(session) {
            return Err(error);
        }
        self.authority.admit_turn_files(owner, session, || {
            permit.send(command);
        })?;
        Ok(())
    }
}
