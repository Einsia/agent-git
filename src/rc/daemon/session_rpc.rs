use super::*;

impl Daemon {
    fn acquire_session_rpc_lease(&mut self, f: &Frame) -> Result<SessionRpcLease, RpcError> {
        let caller = caller_scope(f)?;
        require_role(&caller, f.method())?;
        let session_id = queued_session_id(f)?;
        let (generation, gate) = {
            let live = self
                .sessions
                .get(&session_id)
                .ok_or_else(|| no_such_session(&session_id))?;
            require_same_workspace(&caller, &session_id, &live.info.workspace_id)?;
            if live.ended {
                return Err(no_such_session(&session_id));
            }
            (live.generation, live.rpc_gate.clone())
        };
        let serial = gate.try_lock_owned().map_err(|_| {
            RpcError::new(
                ErrorCode::SessionBusy,
                "that session is already handling another instruction",
            )
            .with_hint("nothing was queued for this request; retry after the earlier reply arrives")
        })?;

        Ok(SessionRpcLease {
            session_id,
            generation,
            serial,
        })
    }

    pub(super) fn prepare_session_rpc_or_wait(
        &mut self,
        f: &Frame,
    ) -> Result<SessionRpcPreparation, RpcError> {
        let lease = self.acquire_session_rpc_lease(f)?;
        self.prepare_session_rpc_with_lease(f, lease)
    }

    /// Test/internal convenience for callers that deliberately do not own the
    /// frame-pump wait path. Production uses `prepare_session_rpc_or_wait` so a
    /// just-started harness does not surface the transient 303 to the browser.
    #[cfg(test)]
    pub(super) fn prepare_session_rpc(
        &mut self,
        f: &Frame,
    ) -> Result<PreparedSessionRpc, RpcError> {
        match self.prepare_session_rpc_or_wait(f)? {
            SessionRpcPreparation::Ready(prepared) => Ok(*prepared),
            SessionRpcPreparation::AwaitingDurableGuardRow(_lease) => {
                Err(Self::durable_guard_pending_error())
            }
        }
    }

    fn prepare_session_rpc_with_lease(
        &mut self,
        f: &Frame,
        lease: SessionRpcLease,
    ) -> Result<SessionRpcPreparation, RpcError> {
        // The Bound wait can outlive several session notes. Re-check every
        // authorization and generation fact under the daemon mutex before
        // creating a command; the retained gate preserves ordering, not stale
        // authority.
        let caller = caller_scope(f)?;
        require_role(&caller, f.method())?;
        let frame_session_id = queued_session_id(f)?;
        if frame_session_id != lease.session_id {
            return Err(RpcError::new(
                ErrorCode::Internal,
                "the retained session RPC lease no longer matches its request",
            ));
        }
        let SessionRpcLease {
            session_id,
            generation,
            serial,
        } = lease;
        let live = self
            .sessions
            .get(&session_id)
            .ok_or_else(|| no_such_session(&session_id))?;
        require_same_workspace(&caller, &session_id, &live.info.workspace_id)?;
        if live.ended || live.generation != generation {
            return Err(no_such_session(&session_id));
        }

        let (operation, guard_sensitive) = match f.method() {
            method::TURN_START => {
                let p: TurnStart = f.params_as()?;
                // `Drive`: adding model input spends the session's capability.
                let d = self.session_channel(&p.session_id, &caller, Need::Drive)?;
                // Arm before the command can enter the supervisor queue. From
                // this point through native write, notification inference, and
                // a late exact response, every crash restarts as Plan until
                // this exact opaque token is resolved.
                let guard_attempt = self.prearm_turn_guard_attempt(&p.session_id)?;
                let (ticket, reply) = crate::rc::ticket::ticket();
                (
                    SessionRpcOperation::Turn {
                        tx: d.tx,
                        command: Command::Turn {
                            message: p.message,
                            by: p.by,
                            guard_attempt: guard_attempt.clone(),
                            reply: ticket,
                        },
                        reply: SessionReceipt(reply),
                        guard_attempt: guard_attempt.clone(),
                    },
                    guard_attempt.is_some(),
                )
            }
            method::TURN_STEER => {
                let p: TurnSteer = f.params_as()?;
                let d = self.session_channel(&p.session_id, &caller, Need::Drive)?;
                let (ticket, reply) = crate::rc::ticket::ticket();
                (
                    SessionRpcOperation::Steer {
                        tx: d.tx,
                        command: Command::Steer {
                            message: p.message,
                            by: p.by,
                            reply: ticket,
                        },
                        reply: SessionReceipt(reply),
                    },
                    false,
                )
            }
            method::SESSION_SET_PERMISSION_MODE => {
                let p: crate::protocol::SessionSetPermissionMode = f.params_as()?;
                // Authorization and durable arming happen after acquiring this
                // session's serial guard, so two mode changes cannot both
                // prepare against the same stale baseline.
                let loosening = self
                    .sessions
                    .get(&p.session_id)
                    .map(|l| p.mode.loosens_from(l.authorization_baseline()))
                    .unwrap_or_else(|| p.mode.loosens_guard());
                let need = if loosening { Need::Drive } else { Need::Brake };
                let d = self.session_channel(&p.session_id, &caller, need)?;
                let supported = crate::rc::harness::capability_of(&d.runtime);
                if !supported.permission_modes.contains(&p.mode) {
                    return Err(RpcError::new(
                        ErrorCode::RuntimeUnavailable,
                        format!("{} cannot express that permission mode", d.runtime),
                    )
                    .with_hint("the picker should only offer what `capabilities` reports"));
                }
                require_owner_if_loosening(Some(&caller), loosening)?;
                if !self.durable_guard_row_complete(&p.session_id) {
                    return Ok(SessionRpcPreparation::AwaitingDurableGuardRow(
                        SessionRpcLease {
                            session_id,
                            generation,
                            serial,
                        },
                    ));
                }
                self.require_durable_guard_row(&p.session_id)?;
                let armed = if p.mode.is_dangerous() {
                    self.arm_danger_before_loosening(&p.session_id)?
                } else {
                    None
                };
                let recovery_token = fresh_shutdown_guard_token(
                    self.roster
                        .sessions
                        .get(&p.session_id)
                        .map(|entry| &entry.guard_attempts),
                );
                let (ticket, reply) = crate::rc::ticket::ticket();
                (
                    SessionRpcOperation::SetPermissionMode {
                        tx: d.tx,
                        command: Command::SetPermissionMode {
                            mode: p.mode,
                            by: p.by,
                            armed,
                            reply: ticket,
                        },
                        reply: SessionReceipt(reply),
                        mode: p.mode,
                        armed,
                        recovery_token,
                    },
                    true,
                )
            }
            method::TURN_INTERRUPT => {
                let p: TurnInterrupt = f.params_as()?;
                // Interrupt is a brake: a dangerous session must remain
                // stoppable by an operator.
                let d = self.session_channel(&p.session_id, &caller, Need::Brake)?;
                let (ticket, reply) = crate::rc::ticket::ticket();
                (
                    SessionRpcOperation::Interrupt {
                        tx: d.tx,
                        command: Command::Interrupt { reply: ticket },
                        reply: SessionReceipt(reply),
                    },
                    false,
                )
            }
            method::APPROVAL_DECIDE => {
                let p: crate::protocol::ApprovalResponse = f.params_as()?;
                let need = match p.decision {
                    crate::protocol::ApprovalDecision::Allow => Need::Drive,
                    crate::protocol::ApprovalDecision::Deny => Need::Brake,
                };
                let d = self.session_channel(&p.session_id, &caller, need)?;
                let changes_session_policy = matches!(
                    (p.decision, p.scope),
                    (
                        crate::protocol::ApprovalDecision::Allow,
                        crate::protocol::ApprovalScope::Session
                    )
                );
                require_owner_if_loosening(Some(&caller), changes_session_policy)?;
                let suggested_mode = changes_session_policy
                    .then(|| {
                        self.sessions.get(&p.session_id).and_then(|live| {
                            live.approval_session_modes.get(&p.approval_id).copied()
                        })
                    })
                    .flatten();
                if changes_session_policy && suggested_mode.is_none() {
                    return Err(RpcError::new(
                        ErrorCode::MalformedFrame,
                        "this approval did not advertise a fully understood session permission-mode effect",
                    )
                    .with_hint("answer it once; a sticky native policy cannot be authorized or persisted without an exact machine-originated mode"));
                }
                if changes_session_policy {
                    if !self.durable_guard_row_complete(&p.session_id) {
                        return Ok(SessionRpcPreparation::AwaitingDurableGuardRow(
                            SessionRpcLease {
                                session_id,
                                generation,
                                serial,
                            },
                        ));
                    }
                    self.require_durable_guard_row(&p.session_id)?;
                }
                let danger = if suggested_mode.is_some_and(|mode| mode.is_dangerous()) {
                    DangerAuthorization::persisted(self.arm_danger_before_loosening(&p.session_id)?)
                } else {
                    DangerAuthorization::NotRequired
                };
                let approval_id = p.approval_id.clone();
                let (ticket, reply) = crate::rc::ticket::ticket();
                (
                    SessionRpcOperation::Approve {
                        tx: d.tx,
                        command: Command::Approve {
                            response: p,
                            // The caller seal belongs to the relayed frame, not to
                            // any browser-controlled params field.
                            caller_is_owner: f.caller.as_ref().is_some_and(|c| c.is_owner()),
                            danger,
                            reply: ticket,
                        },
                        reply: SessionReceipt(reply),
                        approval_id,
                        danger,
                        session_mode: suggested_mode,
                    },
                    changes_session_policy,
                )
            }
            _ => {
                return Err(RpcError::new(
                    ErrorCode::Internal,
                    "not a queued session instruction",
                ));
            }
        };

        if let Some(live) = self
            .sessions
            .get_mut(&session_id)
            .filter(|live| live.generation == generation)
        {
            live.rpc_guard_sensitive = guard_sensitive;
        }

        Ok(SessionRpcPreparation::Ready(Box::new(PreparedSessionRpc {
            session_id,
            generation,
            serial,
            operation,
        })))
    }

    /// Project one completed command back into daemon/roster state, fenced to
    /// the exact live generation that was prepared. The caller still owns that
    /// generation's `rpc_gate`, so this is also the last point before an Ended
    /// tombstone may be removed.
    pub(super) fn complete_session_rpc(
        &mut self,
        session_id: &str,
        generation: u64,
        completion: &SessionRpcCompletion,
    ) -> crate::Result<()> {
        if self
            .sessions
            .get(session_id)
            .is_none_or(|live| live.generation != generation)
        {
            return Ok(());
        }

        // Typed terminal outcomes can beat the supervisor's cross-channel
        // Ended note. Fence the exact generation here, before any fallible
        // durable projection, so resume cannot return a dead Live while its
        // Plan barrier is retrying. Successful completion removes this same
        // tombstone before the RPC gate/response is released; a later Ended
        // note is generation-fenced and harmless.
        if completion.retires_generation() {
            let live = self
                .sessions
                .get_mut(session_id)
                .expect("generation was checked above");
            live.ended = true;
            live.info.status = crate::protocol::SessionStatus::Ended;
        }

        let mut persist_guard = false;
        let mut removed_guard_attempt: Option<(String, roster::GuardAttempt)> = None;
        let mut confirmed_guard_to_clear: Option<String> = None;
        let mut inflight_turn_guard_to_clear: Option<String> = None;
        match completion {
            SessionRpcCompletion::None => {}
            SessionRpcCompletion::Turn {
                guard_attempt,
                accepted_mode,
                confirmation,
                fail_closed,
                ..
            } => {
                if let Some(attempt) = guard_attempt
                    && !*fail_closed
                {
                    if confirmation.is_some() && accepted_mode != &Some(attempt.expected_mode) {
                        anyhow::bail!("accepted turn mode does not match its pre-dispatch guard");
                    }
                    if !matches!(confirmation, Some(TurnStartConfirmation::NotificationOnly)) {
                        let stored = self
                            .roster
                            .sessions
                            .get(session_id)
                            .and_then(|entry| entry.guard_attempts.get(&attempt.token))
                            .ok_or_else(|| {
                                anyhow::anyhow!("turn completion lost its pre-dispatch guard token")
                            })?;
                        if stored.expected_mode != attempt.expected_mode {
                            anyhow::bail!(
                                "turn completion guard token no longer matches its expected mode"
                            );
                        }
                    }
                }
                if let Some(live) = self.sessions.get_mut(session_id) {
                    if *fail_closed {
                        // The supervisor has already proven the harness tree
                        // gone. Plan is therefore an honest restart policy,
                        // not a claim about a still-running native process.
                        live.info.permission_mode = Some(crate::protocol::PermissionMode::Plan);
                        live.pending_mode = None;
                        persist_guard = true;
                    } else if let Some(mode) = accepted_mode {
                        live.info.permission_mode = Some(*mode);
                        live.pending_mode = None;
                        live.info.dangerous = live.info.dangerous || mode.is_dangerous();
                        persist_guard = true;
                    }
                }
                if let Some(attempt) = guard_attempt {
                    inflight_turn_guard_to_clear = Some(attempt.token.clone());
                    let consumed_guard = accepted_mode == &Some(attempt.expected_mode);
                    let inferred =
                        matches!(confirmation, Some(TurnStartConfirmation::NotificationOnly));
                    if *fail_closed {
                        // Keep the pre-dispatch token: Unknown can never prove
                        // whether native consumed its expected mode.
                        let entry =
                            self.roster.sessions.get_mut(session_id).ok_or_else(|| {
                                anyhow::anyhow!("guarded turn lost its roster row")
                            })?;
                        match entry.guard_attempts.get(&attempt.token) {
                            Some(stored) if stored.expected_mode != attempt.expected_mode => {
                                anyhow::bail!(
                                    "guarded turn token changed its expected mode before fail-closed projection"
                                );
                            }
                            Some(_) => {}
                            None => {
                                entry.guard_attempts.insert(
                                    attempt.token.clone(),
                                    roster::GuardAttempt {
                                        expected_mode: attempt.expected_mode,
                                        observed: false,
                                    },
                                );
                            }
                        }
                        persist_guard = true;
                    } else if consumed_guard && inferred {
                        let armed_and_observed = self
                            .roster
                            .sessions
                            .get(session_id)
                            .and_then(|entry| entry.guard_attempts.get(&attempt.token))
                            .is_some_and(|stored| {
                                stored.expected_mode == attempt.expected_mode && stored.observed
                            });
                        let confirmed = self
                            .sessions
                            .get(session_id)
                            .and_then(|live| live.confirmed_turn_guards.get(&attempt.token))
                            .is_some_and(|mode| *mode == attempt.expected_mode);
                        if !armed_and_observed && !confirmed {
                            anyhow::bail!(
                                "notification-only turn completion lost its pre-dispatch guard token"
                            );
                        }
                        if confirmed {
                            confirmed_guard_to_clear = Some(attempt.token.clone());
                        }
                        persist_guard = true;
                    } else {
                        // Exact acceptance, explicit refusal, retryable local
                        // rejection, or an initial-prompt coalescing path all
                        // prove this pre-armed token no longer represents an
                        // ambiguous native guard write.
                        let stored = self
                            .roster
                            .sessions
                            .get(session_id)
                            .and_then(|entry| entry.guard_attempts.get(&attempt.token))
                            .ok_or_else(|| {
                                anyhow::anyhow!("turn completion lost its pre-dispatch guard token")
                            })?;
                        if stored.expected_mode != attempt.expected_mode {
                            anyhow::bail!(
                                "turn completion guard token no longer matches its expected mode"
                            );
                        }
                        let removed = self
                            .roster
                            .sessions
                            .get_mut(session_id)
                            .expect("guard entry was checked above")
                            .guard_attempts
                            .remove(&attempt.token)
                            .expect("guard token was checked above");
                        removed_guard_attempt = Some((attempt.token.clone(), removed));
                        persist_guard = true;
                    }
                }
            }
            SessionRpcCompletion::PermissionMode {
                mode,
                applied,
                rollback_arm,
                recovery_token,
                ..
            } => {
                if let Some(token) = recovery_token {
                    if applied.is_some() || rollback_arm.is_some() {
                        anyhow::bail!(
                            "an ambiguous permission-mode completion cannot also be applied or refused"
                        );
                    }
                    if !token.starts_with(roster::SHUTDOWN_GUARD_PREFIX) {
                        anyhow::bail!(
                            "permission-mode recovery token is outside the reserved shutdown namespace"
                        );
                    }
                    let Some(entry) = self.roster.sessions.get(session_id) else {
                        anyhow::bail!(
                            "ambiguous permission-mode completion lost its durable roster row"
                        );
                    };
                    match entry.guard_attempts.get(token) {
                        Some(attempt)
                            if attempt.expected_mode == crate::protocol::PermissionMode::Plan
                                && !attempt.observed => {}
                        Some(_) => anyhow::bail!(
                            "permission-mode recovery token changed payload before persistence retry"
                        ),
                        None => {}
                    }

                    // The supervisor proved the uncertain native process tree
                    // terminated before releasing this typed outcome. Keep a
                    // synthetic token as the monotonic floor so an older loose
                    // permission frame already queued on the other channel
                    // cannot undo Plan while persistence retries.
                    let live = self
                        .sessions
                        .get_mut(session_id)
                        .expect("generation was checked above");
                    live.info.permission_mode = Some(crate::protocol::PermissionMode::Plan);
                    live.pending_mode = None;
                    let entry = self
                        .roster
                        .sessions
                        .get_mut(session_id)
                        .expect("roster row was checked above");
                    entry.permission_mode = Some(crate::protocol::PermissionMode::Plan);
                    entry
                        .guard_attempts
                        .entry(token.clone())
                        .or_insert_with(|| roster::GuardAttempt {
                            expected_mode: crate::protocol::PermissionMode::Plan,
                            observed: false,
                        });
                    persist_guard = true;
                }
                if let Some(arm) = *rollback_arm {
                    self.disarm_danger(session_id, arm);
                }
                if let Some(applied) = applied {
                    let effective = matches!(applied, crate::protocol::PermissionApply::Immediate);
                    if let Some(live) = self.sessions.get_mut(session_id) {
                        if effective {
                            live.info.permission_mode = Some(*mode);
                            live.pending_mode = None;
                        } else {
                            live.pending_mode = Some(*mode);
                        }
                        live.info.dangerous = live.info.dangerous || mode.is_dangerous();
                    }
                    persist_guard = true;
                }
            }
            SessionRpcCompletion::Approval {
                approval_id,
                resolved,
                effective_mode,
                fail_closed,
                rollback_arm,
                ..
            } => {
                if let Some(arm) = *rollback_arm {
                    self.disarm_danger(session_id, arm);
                }
                if let Some(live) = self.sessions.get_mut(session_id) {
                    if *resolved {
                        live.approval_session_modes.remove(approval_id);
                    }
                    if *fail_closed {
                        // The supervisor releases Unknown only after proving
                        // the harness tree gone, so Plan is an honest restart
                        // policy rather than a claim about a live process.
                        live.info.permission_mode = Some(crate::protocol::PermissionMode::Plan);
                        live.pending_mode = None;
                        persist_guard = true;
                    } else if let Some(mode) = effective_mode {
                        live.info.permission_mode = Some(*mode);
                        live.pending_mode = None;
                        live.info.dangerous = live.info.dangerous || mode.is_dangerous();
                        persist_guard = true;
                    }
                }
            }
        }

        if persist_guard && let Err(error) = self.persist_session_state_fail_closed(session_id) {
            if let Some((token, attempt)) = removed_guard_attempt
                && let Some(entry) = self.roster.sessions.get_mut(session_id)
            {
                entry.guard_attempts.insert(token, attempt);
            }
            return Err(error);
        }
        if let Some(token) = confirmed_guard_to_clear
            && let Some(live) = self.sessions.get_mut(session_id)
        {
            live.confirmed_turn_guards.remove(&token);
        }
        if let Some(token) = inflight_turn_guard_to_clear
            && let Some(live) = self.sessions.get_mut(session_id)
            && live.inflight_turn_guard.as_deref() == Some(token.as_str())
        {
            live.inflight_turn_guard = None;
        }
        // A known successful guard mutation is not eligible for an ACK until
        // the exact restart state above is durable. Keep this flag and the
        // caller-owned rpc_gate armed across any failed save/retry.
        if let Some(live) = self.sessions.get_mut(session_id) {
            live.rpc_guard_sensitive = false;
        }
        if self
            .sessions
            .get(session_id)
            .is_some_and(|live| live.generation == generation && live.ended)
        {
            self.remove_session_generation(session_id, generation);
        }
        Ok(())
    }

    /// Persist the strictest restart mode for RPCs that remained TAKEN past the
    /// entire graceful-stop budget.
    ///
    /// At that boundary the harness may have applied a mode change while its
    /// receipt/projection was lost, so retaining the predecessor can silently
    /// resurrect a looser session after restart. Only set-mode and an approval
    /// that explicitly changes session policy set `rpc_guard_sensitive`;
    /// ordinary turn/steer/interrupt/one-shot approval stalls do not rewrite
    /// the user's durable mode.
    pub(super) fn inflight_guard_sensitive_session_rpcs(&self) -> Vec<HardStopGuard> {
        self.sessions
            .iter()
            .filter(|(_, live)| live.rpc_guard_sensitive && live.rpc_gate.try_lock().is_err())
            .map(|(session_id, live)| HardStopGuard {
                session_id: session_id.clone(),
                generation: live.generation,
                token: fresh_shutdown_guard_token(
                    self.roster
                        .sessions
                        .get(session_id)
                        .map(|entry| &entry.guard_attempts),
                ),
                dangerous: live.info.dangerous,
            })
            .collect()
    }

    /// Persist a frozen hard-deadline decision.
    ///
    /// `guards` is captured once at the deadline rather than rediscovered
    /// on every disk retry. A late worker may release its gate between attempts;
    /// the daemon must still make the stricter restart state durable before it
    /// is allowed to cancel the remaining task set.
    pub(super) fn persist_fail_closed_session_rpcs(
        &mut self,
        guards: &[HardStopGuard],
    ) -> crate::Result<()> {
        if guards.is_empty() {
            return Ok(());
        }

        // Validate the complete frozen batch before mutating either in-memory
        // projection. Otherwise a missing final row could leave the preceding
        // sessions tightened without the one atomic save that makes the batch
        // authoritative after a crash. Once this exact token is present, it is
        // also the retry witness: the original generation may naturally finish
        // and remove its `Live` row while a failed disk write is backing off.
        for guard in guards {
            let Some(entry) = self.roster.sessions.get(&guard.session_id) else {
                anyhow::bail!(
                    "guard-sensitive session {} has no durable roster row at hard stop",
                    guard.session_id
                );
            };
            match entry.guard_attempts.get(&guard.token) {
                Some(attempt)
                    if attempt.expected_mode == crate::protocol::PermissionMode::Plan
                        && !attempt.observed => {}
                Some(_) => anyhow::bail!(
                    "hard-stop guard {} changed payload before persistence retry",
                    guard.token
                ),
                None => {
                    let Some(_) = self
                        .sessions
                        .get(&guard.session_id)
                        .filter(|live| live.generation == guard.generation)
                    else {
                        anyhow::bail!(
                            "guard-sensitive session {} generation {} disappeared before hard-stop persistence",
                            guard.session_id,
                            guard.generation
                        );
                    };
                }
            }
        }

        // The synthetic token is the durable, monotonic floor. Do not roll
        // these mutations back if both persistence destinations fail: later
        // projections and the retry itself must continue to see Plan while the
        // worker and its per-session gate remain alive.
        for guard in guards {
            let already_armed = self.roster.sessions[&guard.session_id]
                .guard_attempts
                .contains_key(&guard.token);
            if let Some(live) = self
                .sessions
                .get_mut(&guard.session_id)
                .filter(|live| live.generation == guard.generation)
            {
                live.info.permission_mode = Some(crate::protocol::PermissionMode::Plan);
                live.pending_mode = None;
            } else if !already_armed {
                unreachable!("unarmed hard-stop generation was validated above");
            }
            let entry = self
                .roster
                .sessions
                .get_mut(&guard.session_id)
                .expect("hard-stop roster row was validated above");
            entry.permission_mode = Some(crate::protocol::PermissionMode::Plan);
            entry.ever_dangerous = entry.ever_dangerous || guard.dangerous;
            entry
                .guard_attempts
                .entry(guard.token.clone())
                .or_insert_with(|| roster::GuardAttempt {
                    expected_mode: crate::protocol::PermissionMode::Plan,
                    observed: false,
                });
        }
        self.roster.save_fail_closed()
    }

    #[cfg(test)]
    pub(super) fn fail_closed_inflight_session_rpcs(&mut self) -> crate::Result<()> {
        let guards = self.inflight_guard_sensitive_session_rpcs();
        self.persist_fail_closed_session_rpcs(&guards)
    }
}

impl PendingSessionRpc {
    /// Finish projection for a command that was already taken when its bounded
    /// RPC response window expired. The response has already been queued, but
    /// this task deliberately retains the per-session guard until the actual
    /// executor closes its receipt.
    pub(super) async fn finish(self, daemon: Arc<Mutex<Daemon>>) {
        let PendingSessionRpc {
            session_id,
            generation,
            serial,
            operation,
        } = self;
        let completion = match operation {
            PendingSessionRpcOperation::Steer(mut reply) => {
                let _ = reply.wait_until_closed().await;
                SessionRpcCompletion::None
            }
            PendingSessionRpcOperation::Interrupt(mut reply) => {
                let _ = reply.wait_until_closed().await;
                SessionRpcCompletion::None
            }
            PendingSessionRpcOperation::Approve {
                mut reply,
                approval_id,
                danger,
                session_mode,
            } => match reply.wait_until_closed().await {
                Ok(Ok(outcome)) => {
                    let Some((completion, _response)) =
                        project_approval_outcome(outcome, approval_id, session_mode, danger)
                    else {
                        eprintln!(
                            "agitd: a late approval result contradicted its trusted mode; retaining its RPC gate until hard-stop persistence"
                        );
                        return std::future::pending::<()>().await;
                    };
                    completion
                }
                Ok(Err(_)) if session_mode.is_some() => {
                    eprintln!(
                        "agitd: a taken session approval returned no typed outcome; retaining its RPC gate until hard-stop persistence"
                    );
                    return std::future::pending::<()>().await;
                }
                Ok(Err(_)) => SessionRpcCompletion::Approval {
                    approval_id,
                    resolved: false,
                    effective_mode: None,
                    fail_closed: false,
                    rollback_arm: danger.arm(),
                    retire_generation: false,
                },
                Err(_) if session_mode.is_some() => {
                    eprintln!(
                        "agitd: a taken session approval lost its typed supervisor outcome; retaining its RPC gate until hard-stop persistence"
                    );
                    std::future::pending::<SessionRpcCompletion>().await
                }
                Err(_) => SessionRpcCompletion::None,
            },
        };
        complete_prepared_session_rpc(daemon, session_id, generation, serial, completion).await;
    }
}

impl SessionRpcLease {
    pub(super) async fn serve_when_bound(self, wait: SessionRpcBoundWait) {
        self.serve_when_bound_within(wait, DURABLE_GUARD_BIND_TIMEOUT)
            .await;
    }

    pub(super) async fn serve_when_bound_within(
        self,
        wait: SessionRpcBoundWait,
        within: std::time::Duration,
    ) {
        let SessionRpcBoundWait {
            daemon,
            outbound,
            id,
            frame,
            connection_epoch,
            mut stop,
        } = wait;
        let session_id = self.session_id.clone();
        let generation = self.generation;
        let mut lease = Some(self);
        let deadline = tokio::time::Instant::now() + within;

        loop {
            let preparation = {
                let mut state = daemon.lock().await;
                if !connection_epoch_is_current(&state.settlement, connection_epoch) {
                    Some(Err(RpcError::new(
                        ErrorCode::SessionBusy,
                        "the hub connection changed before this session finished binding",
                    )
                    .with_hint(
                        "nothing was queued; retry the instruction on the current connection",
                    )))
                } else if !state
                    .sessions
                    .get(&session_id)
                    .is_some_and(|live| live.generation == generation && !live.ended)
                {
                    Some(Err(no_such_session(&session_id)))
                } else if state.durable_guard_row_complete(&session_id) {
                    Some(state.prepare_session_rpc_with_lease(
                        &frame,
                        lease.take().expect("the Bound waiter owns one lease"),
                    ))
                } else {
                    None
                }
            };

            match preparation {
                Some(Ok(SessionRpcPreparation::Ready(prepared))) => {
                    (*prepared).serve(daemon, outbound, id, stop).await;
                    return;
                }
                Some(Ok(SessionRpcPreparation::AwaitingDurableGuardRow(next))) => {
                    // The complete row cannot regress while the daemon mutex is
                    // held, but retain the lease and keep the branch explicit
                    // if a future roster implementation makes it transient.
                    lease = Some(next);
                }
                Some(Err(error)) => {
                    if let Some(lease) = lease.take() {
                        release_unprepared_session_rpc(daemon.clone(), lease).await;
                    }
                    let _ = outbound.send(Frame::error_response(id, error));
                    return;
                }
                None => {}
            }

            let now = tokio::time::Instant::now();
            if now >= deadline {
                let error = RpcError::new(
                    ErrorCode::SessionBusy,
                    "the harness did not report its native restart identity in time",
                )
                .with_hint(
                    "nothing was queued; the session may still be starting, so retry after it appears online",
                );
                if let Some(lease) = lease.take() {
                    release_unprepared_session_rpc(daemon.clone(), lease).await;
                }
                let _ = outbound.send(Frame::error_response(id, error));
                return;
            }
            let pause = DURABLE_GUARD_BIND_POLL.min(deadline.saturating_duration_since(now));
            tokio::select! {
                _ = rpc_stop_requested(&mut stop) => {
                    let error = RpcError::new(
                        ErrorCode::SessionBusy,
                        "the daemon stopped before this session finished binding",
                    )
                    .with_hint("nothing was queued, so it is safe to retry after reconnecting");
                    if let Some(lease) = lease.take() {
                        release_unprepared_session_rpc(daemon.clone(), lease).await;
                    }
                    let _ = outbound.send(Frame::error_response(id, error));
                    return;
                }
                _ = tokio::time::sleep(pause) => {}
            }
        }
    }
}

impl PreparedSessionRpc {
    pub(super) async fn execute(
        self,
        daemon: Arc<Mutex<Daemon>>,
        stop: &mut tokio::sync::watch::Receiver<bool>,
    ) -> ExecutedSessionRpc {
        let PreparedSessionRpc {
            session_id,
            generation,
            serial,
            operation,
        } = self;

        match operation {
            SessionRpcOperation::Turn {
                tx,
                command,
                mut reply,
                guard_attempt,
            } => {
                if let Err(error) = enqueue_within(&tx, command, SESSION_REPLY_TIMEOUT, stop).await
                {
                    return finish_prepared_session_rpc(
                        daemon,
                        session_id,
                        generation,
                        serial,
                        SessionRpcCompletion::Turn {
                            guard_attempt,
                            accepted_mode: None,
                            confirmation: None,
                            fail_closed: false,
                            retire_generation: false,
                        },
                        Err(error),
                    )
                    .await;
                }
                match reply_within_state(reply.get_mut(), stop).await {
                    ReplyWait::Done(result) => {
                        let outcome = match result {
                            Ok(outcome) => outcome,
                            Err(error) => match reply.abandon() {
                                crate::rc::ticket::Abandon::NeverRan => {
                                    TurnStartOutcome::RetryableNotAccepted {
                                        message: error.message,
                                    }
                                }
                                crate::rc::ticket::Abandon::AlreadyTaken => {
                                    eprintln!(
                                        "agitd: a taken turn/start lost its typed supervisor outcome; retaining its RPC gate until hard-stop persistence"
                                    );
                                    std::future::pending::<TurnStartOutcome>().await
                                }
                            },
                        };
                        let (completion, response) =
                            project_turn_start_outcome(outcome, guard_attempt.clone());
                        finish_prepared_session_rpc(
                            daemon, session_id, generation, serial, completion, response,
                        )
                        .await
                    }
                    ReplyWait::InFlight(_error) => {
                        // A taken turn is never safe to answer with the generic
                        // early outcome-unknown path: it may carry a sticky mode
                        // whose Plan fallback must become durable first. The
                        // driver itself has a bounded native deadline and an
                        // Unknown path that kills the harness tree, so retain
                        // this worker/gate until the typed receipt arrives. At
                        // daemon hard-stop the existing deadline code persists
                        // Plan before aborting this worker.
                        let outcome = match reply.wait_until_closed().await {
                            Ok(Ok(outcome)) => outcome,
                            Ok(Err(error)) => TurnStartOutcome::Unknown {
                                message: format!(
                                    "the session failed after taking turn/start without a classified outcome: {error}"
                                ),
                                attempted_mode: None,
                            },
                            Err(_) => {
                                eprintln!(
                                    "agitd: a taken turn/start receipt closed without termination proof; retaining its RPC gate until hard-stop persistence"
                                );
                                std::future::pending::<TurnStartOutcome>().await
                            }
                        };
                        let (completion, response) =
                            project_turn_start_outcome(outcome, guard_attempt);
                        finish_prepared_session_rpc(
                            daemon, session_id, generation, serial, completion, response,
                        )
                        .await
                    }
                }
            }
            SessionRpcOperation::Steer {
                tx,
                command,
                mut reply,
            } => {
                if let Err(error) = enqueue_within(&tx, command, SESSION_REPLY_TIMEOUT, stop).await
                {
                    return finish_prepared_session_rpc(
                        daemon,
                        session_id,
                        generation,
                        serial,
                        SessionRpcCompletion::None,
                        Err(error),
                    )
                    .await;
                }
                match reply_within_state(reply.get_mut(), stop).await {
                    ReplyWait::Done(result) => {
                        let response = result.map(|delivery| {
                            serde_json::to_value(TurnSteerResult { delivery }).unwrap()
                        });
                        finish_prepared_session_rpc(
                            daemon,
                            session_id,
                            generation,
                            serial,
                            SessionRpcCompletion::None,
                            response,
                        )
                        .await
                    }
                    ReplyWait::InFlight(error) => ExecutedSessionRpc {
                        response: Err(error),
                        pending: Some(PendingSessionRpc {
                            session_id,
                            generation,
                            serial,
                            operation: PendingSessionRpcOperation::Steer(reply),
                        }),
                    },
                }
            }
            SessionRpcOperation::SetPermissionMode {
                tx,
                command,
                mut reply,
                mode,
                armed,
                recovery_token,
            } => {
                if let Err(error) = enqueue_within(&tx, command, SESSION_REPLY_TIMEOUT, stop).await
                {
                    return finish_prepared_session_rpc(
                        daemon,
                        session_id,
                        generation,
                        serial,
                        SessionRpcCompletion::PermissionMode {
                            mode,
                            applied: None,
                            rollback_arm: armed,
                            recovery_token: None,
                            retire_generation: false,
                        },
                        Err(error),
                    )
                    .await;
                }
                let outcome = match reply_within_state(reply.get_mut(), stop).await {
                    ReplyWait::Done(Ok(outcome)) => outcome,
                    ReplyWait::Done(Err(error)) => match reply.abandon() {
                        crate::rc::ticket::Abandon::NeverRan => {
                            return finish_prepared_session_rpc(
                                daemon,
                                session_id,
                                generation,
                                serial,
                                SessionRpcCompletion::PermissionMode {
                                    mode,
                                    applied: None,
                                    rollback_arm: armed,
                                    recovery_token: None,
                                    retire_generation: false,
                                },
                                Err(error),
                            )
                            .await;
                        }
                        crate::rc::ticket::Abandon::AlreadyTaken => {
                            eprintln!(
                                "agitd: a taken permission-mode command lost its typed supervisor outcome; retaining its RPC gate until hard-stop persistence"
                            );
                            std::future::pending::<PermissionModeOutcome>().await
                        }
                    },
                    ReplyWait::InFlight(_error) => match reply.wait_until_closed().await {
                        Ok(Ok(outcome)) => outcome,
                        Ok(Err(error)) => {
                            eprintln!(
                                "agitd: a late permission-mode command returned no typed outcome ({error:#}); retaining its RPC gate until hard-stop persistence"
                            );
                            std::future::pending::<PermissionModeOutcome>().await
                        }
                        Err(_) => {
                            eprintln!(
                                "agitd: a taken permission-mode receipt closed without termination proof; retaining its RPC gate until hard-stop persistence"
                            );
                            std::future::pending::<PermissionModeOutcome>().await
                        }
                    },
                };
                let (completion, response) =
                    project_permission_mode_outcome(outcome, mode, armed, recovery_token);
                finish_prepared_session_rpc(
                    daemon, session_id, generation, serial, completion, response,
                )
                .await
            }
            SessionRpcOperation::Interrupt {
                tx,
                command,
                mut reply,
            } => {
                if let Err(error) = enqueue_within(&tx, command, SESSION_REPLY_TIMEOUT, stop).await
                {
                    return finish_prepared_session_rpc(
                        daemon,
                        session_id,
                        generation,
                        serial,
                        SessionRpcCompletion::None,
                        Err(error),
                    )
                    .await;
                }
                match reply_within_state(reply.get_mut(), stop).await {
                    ReplyWait::Done(result) => {
                        let response = result.map(|()| {
                            serde_json::to_value(TurnInterruptResult::default()).unwrap()
                        });
                        finish_prepared_session_rpc(
                            daemon,
                            session_id,
                            generation,
                            serial,
                            SessionRpcCompletion::None,
                            response,
                        )
                        .await
                    }
                    ReplyWait::InFlight(error) => ExecutedSessionRpc {
                        response: Err(error),
                        pending: Some(PendingSessionRpc {
                            session_id,
                            generation,
                            serial,
                            operation: PendingSessionRpcOperation::Interrupt(reply),
                        }),
                    },
                }
            }
            SessionRpcOperation::Approve {
                tx,
                command,
                mut reply,
                approval_id,
                danger,
                session_mode,
            } => {
                if let Err(error) = enqueue_within(&tx, command, SESSION_REPLY_TIMEOUT, stop).await
                {
                    return finish_prepared_session_rpc(
                        daemon,
                        session_id,
                        generation,
                        serial,
                        SessionRpcCompletion::Approval {
                            approval_id,
                            resolved: false,
                            effective_mode: None,
                            fail_closed: false,
                            rollback_arm: danger.arm(),
                            retire_generation: false,
                        },
                        Err(error),
                    )
                    .await;
                }
                match reply_within_state(reply.get_mut(), stop).await {
                    ReplyWait::Done(result) => {
                        let (completion, response) = match result {
                            Ok(outcome) => {
                                let Some(projected) = project_approval_outcome(
                                    outcome,
                                    approval_id,
                                    session_mode,
                                    danger,
                                ) else {
                                    eprintln!(
                                        "agitd: an approval result contradicted its trusted mode; retaining its RPC gate until hard-stop persistence"
                                    );
                                    return std::future::pending::<ExecutedSessionRpc>().await;
                                };
                                projected
                            }
                            Err(error) => match reply.abandon() {
                                crate::rc::ticket::Abandon::NeverRan => (
                                    SessionRpcCompletion::Approval {
                                        approval_id,
                                        resolved: false,
                                        effective_mode: None,
                                        fail_closed: false,
                                        rollback_arm: danger.arm(),
                                        retire_generation: false,
                                    },
                                    map_approval_reply(Err(error)),
                                ),
                                crate::rc::ticket::Abandon::AlreadyTaken
                                    if session_mode.is_some() =>
                                {
                                    eprintln!(
                                        "agitd: a taken session approval lost its typed supervisor outcome; retaining its RPC gate until hard-stop persistence"
                                    );
                                    return std::future::pending::<ExecutedSessionRpc>().await;
                                }
                                crate::rc::ticket::Abandon::AlreadyTaken => {
                                    (SessionRpcCompletion::None, map_approval_reply(Err(error)))
                                }
                            },
                        };
                        finish_prepared_session_rpc(
                            daemon, session_id, generation, serial, completion, response,
                        )
                        .await
                    }
                    ReplyWait::InFlight(_error) if session_mode.is_some() => {
                        // A sticky approval may still be waiting for process
                        // tree termination after an ambiguous native write.
                        // Unlike an ordinary one-shot card, it cannot receive
                        // an early error: Plan and the termination proof must
                        // both cross the durability barrier first.
                        let outcome = match reply.wait_until_closed().await {
                            Ok(Ok(outcome)) => outcome,
                            Ok(Err(_)) | Err(_) => {
                                eprintln!(
                                    "agitd: a taken session approval closed without a typed outcome; retaining its RPC gate until hard-stop persistence"
                                );
                                return std::future::pending::<ExecutedSessionRpc>().await;
                            }
                        };
                        let Some((completion, response)) =
                            project_approval_outcome(outcome, approval_id, session_mode, danger)
                        else {
                            eprintln!(
                                "agitd: a delayed approval result contradicted its trusted mode; retaining its RPC gate until hard-stop persistence"
                            );
                            return std::future::pending::<ExecutedSessionRpc>().await;
                        };
                        finish_prepared_session_rpc(
                            daemon, session_id, generation, serial, completion, response,
                        )
                        .await
                    }
                    ReplyWait::InFlight(error) => ExecutedSessionRpc {
                        response: map_approval_reply(Err(error)),
                        pending: Some(PendingSessionRpc {
                            session_id,
                            generation,
                            serial,
                            operation: PendingSessionRpcOperation::Approve {
                                reply,
                                approval_id,
                                danger,
                                session_mode,
                            },
                        }),
                    },
                }
            }
        }
    }

    pub(super) async fn serve(
        self,
        daemon: Arc<Mutex<Daemon>>,
        outbound: crate::rc::outbound::OutboundTx,
        id: crate::protocol::RequestId,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) {
        let ExecutedSessionRpc { response, pending } =
            self.execute(daemon.clone(), &mut stop).await;
        // Replies are request-id fenced and the outbound reply lane deliberately
        // survives reconnects. Enqueue exactly once even if the asking socket
        // turned over while the side effect ran; losing this receipt would
        // invite a retry of a command that may already have happened.
        let out = match response {
            Ok(value) => Frame::response(id, value),
            Err(error) => Frame::error_response(id, error),
        };
        let _ = outbound.send(out);

        // A TAKEN command can outlive its bounded response window. Its caller
        // already has the explicit outcome-unknown response, but the same
        // tracked worker keeps the per-session gate until the real executor
        // closes. No detached waiter is allowed to survive daemon shutdown.
        if let Some(pending) = pending {
            pending.finish(daemon).await;
        }
    }
}
