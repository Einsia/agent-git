use super::*;

/// The bound on the reaper / force-kill threads finishing up for **the whole batch of
/// terminals** at shutdown.
///
/// One bound covers the batch: signal every terminal first, then wait for them one by one under
/// this single deadline, so the grace period does not grow with the number of terminals.
const TERMINAL_CLEANUP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

impl Daemon {
    pub async fn run(opts: Options) -> crate::Result<()> {
        let identity = crate::rc::build_identity::DaemonIdentity::current()?;
        let admission = crate::rc::admission::Admission::default();
        let controller = crate::rc::peers::controller()?;
        // Internal notes, session → daemon. Capacity is generous: a session sends only a few
        // over its whole life.
        let (notes_tx, mut notes_rx) = mpsc::channel::<SessionNote>(256);
        let terminal_delivery_blockers =
            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let initial_authority = SettlementState {
            local_owner: true,
            epoch: 1,
            session_start_idempotency_v1: true,
            ..Default::default()
        };
        let (settlement_tx, _) = tokio::sync::watch::channel(initial_authority);
        // A fail-closed fallback from an earlier hard stop is authoritative.
        // `try_load` refuses launch unless it can promote that snapshot and
        // durably remove the fallback, preventing a stale snapshot from
        // rolling later roster updates back on another restart.
        let roster = Roster::try_load()?;
        // A vault that exists but cannot be unlocked must not start a daemon that pretends to
        // be filtering.
        let secret_filter = crate::domain::secret_filter::MatcherHandle::load_default()?;
        let d = Arc::new(Mutex::new(Daemon {
            identity: identity.clone(),
            deferred: vec![],
            deferred_slot: None,
            replay_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(REPLAY_SLOTS)),
            outbound: None,
            opts,
            journal: Journal::restored(),
            mirror: Mirror::load(),
            roster,
            sessions: HashMap::new(),
            latest_session_generations: HashMap::new(),
            opening_sessions: HashMap::new(),
            watches: HashMap::new(),
            terminals: HashMap::new(),
            terminal_delivery_blockers: terminal_delivery_blockers.clone(),
            term_tx: None,
            online: false,
            secret_filter: secret_filter.clone(),
            settlement: settlement_tx.clone(),
            started_at: std::time::Instant::now(),
            notes: notes_tx,
            grants: crate::rc::grants::Grants::load(),
            watch_generation: 0,
            session_generation: 0,
            confinement: HashMap::new(),
        }));

        // Control socket on a blocking thread: `agit rc status` must work even
        // if the async side is wedged talking to an unreachable hub.
        let ctl = control::listen()?;
        // Unix publication and lifetime ownership belong to the listener. Its worker retains
        // the lock until process exit; no shutdown unlink can erase a replacement's state.
        #[cfg(windows)]
        control::write_pidfile()?;
        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let (safe_stop_tx, mut safe_stop_rx) = mpsc::channel::<restart::SafeStopRequest>(1);
        {
            let d = d.clone();
            let stop_tx = stop_tx.clone();
            let secret_filter = secret_filter.clone();
            std::thread::spawn(move || {
                for stream in ctl.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let d = d.clone();
                    let stop_tx = stop_tx.clone();
                    let secret_filter = secret_filter.clone();
                    let safe_stop_tx = safe_stop_tx.clone();
                    let (written_tx, written_rx) = tokio::sync::oneshot::channel();
                    let _ = control::serve_one(&mut stream, move |req| match req {
                        control::Request::Status => {
                            // The control socket lives on a blocking thread on
                            // purpose: `agit rc status` must answer even when
                            // the async side is stuck dialling an unreachable
                            // hub. So take the lock without an executor, and
                            // rather than block forever, give up with a usable
                            // message — a status command that hangs is worse
                            // than one that says the daemon is busy.
                            let deadline =
                                std::time::Instant::now() + std::time::Duration::from_secs(2);
                            loop {
                                if let Ok(g) = d.try_lock() {
                                    break control::Reply::Status(g.status());
                                }
                                if std::time::Instant::now() > deadline {
                                    break control::Reply::Error {
                                        message: "the daemon is busy and did not answer within 2s; retry, or `agit rc stop` if it stays wedged".into(),
                                    };
                                }
                                std::thread::sleep(std::time::Duration::from_millis(20));
                            }
                        }
                        control::Request::Stop => {
                            let _ = stop_tx.try_send(());
                            control::Reply::Stopping
                        }
                        control::Request::StopIfIdle {
                            instance_id,
                            build_id,
                        } => {
                            let (reply, received) = std::sync::mpsc::sync_channel(1);
                            let budget = std::time::Duration::from_secs(2);
                            let request = restart::SafeStopRequest {
                                instance_id,
                                build_id,
                                deadline: std::time::Instant::now() + budget,
                                reply,
                                written: written_rx,
                            };
                            if safe_stop_tx.try_send(request).is_err() {
                                control::Reply::Busy {
                                    blockers: vec!["safe restart coordinator is busy".into()],
                                }
                            } else {
                                received.recv_timeout(budget).unwrap_or_else(|_| {
                                    control::Reply::Busy {
                                        blockers: vec![
                                            "daemon did not reach a safe restart boundary".into(),
                                        ],
                                    }
                                })
                            }
                        }
                        control::Request::ReloadSecrets => match secret_filter.reload_default() {
                            Ok(status) => control::Reply::SecretsReloaded {
                                generation: status.generation,
                                rules: status.rules,
                            },
                            Err(e) => control::Reply::Error {
                                message: format!("{e:#}"),
                            },
                        },
                    });
                    let _ = written_tx.send(());
                }
            });
        }

        // Frames from sessions → journal → link.
        let (frames_tx, mut frames_rx) = mpsc::channel::<Frame>(4096);
        // Outbound splits in two: one lane for replies, one for replayable stream events.
        // The test is "can it be recovered once lost", and on the taking side replies come
        // first — see `rc::outbound`.
        let (out_tx, _out_rx) = crate::rc::outbound::channel();
        d.lock().await.outbound = Some(out_tx.clone());
        // **The tail of frames still owed.**
        //
        // When the event queue is full and frames get dropped, two kinds must not be lost
        // (terminal exit, the gap notice), and neither may cut ahead of an earlier frame
        // on the same stream. The only way to satisfy both is "queue at the tail and wait
        // for capacity" — and that waiting has to be done by a separate task: the moment
        // the main loop stops, link events, watermark persistence and Ctrl-C stop with it
        // (see the comment on `out_tx.send` below).
        //
        // The channel does not take "full" as a reason to drop terminal.exited; the first
        // frame entering the tail pauses admission of new terminals, and each existing
        // terminal leaves at most one gap and one exit, so production stays bounded by
        // MAX_TERMINALS. `ack` comes back only after the frame is **really queued in the
        // event FIFO**; until then the whole terminal stream stays sealed and a later,
        // larger seq cannot take its place.
        let (tail_tx, tail_rx) = mpsc::unbounded_channel::<Frame>();
        let (tail_ack_tx, mut tail_ack_rx) = mpsc::unbounded_channel::<String>();
        let mut ordered_tail = OrderedTail::new(tail_tx, terminal_delivery_blockers);
        let tail_out = out_tx.clone();
        let tail_task = tokio::spawn(crate::rc::outbound::drain_ordered(
            tail_out,
            tail_rx,
            tail_ack_tx,
        ));
        // The seq watermark persists periodically: a hard kill loses at most one
        // `WATERMARK_DEBOUNCE_MS` window of counting, and on reconnect the hub's
        // `persisted_seq` pushes it back up, so that loss is safe.
        let mut flush = tokio::time::interval(std::time::Duration::from_millis(
            crate::rc::journal::WATERMARK_DEBOUNCE_MS,
        ));
        let (_link_ev_tx, mut link_ev_rx) = mpsc::channel::<link::LinkEvent>(256);

        // **Ctrl-C needs someone waiting on it the whole time, not a fresh one built each
        // select round.**
        //
        // `tokio::select!` builds every branch's future each round and drops them all when
        // it completes, so `ctrl_c()` is a new `Signal` every round — and tokio marks the
        // "current version" as already seen when it registers the listener. The signal
        // driver runs on another worker and immediately consumes that pending notification
        // and advances the version; so a Ctrl-C the user presses while the main loop is
        // **executing some branch body** falls between two `Signal`s and is lost for good.
        //
        // Worse, tokio has already taken over the OS default: that keystroke neither exits
        // nor kills the process any more, and the daemon just plays dead.
        //
        // A task that waits exactly once turns it into a stop — registered once, waiting
        // from then on.
        {
            let stop_tx = stop_tx.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    let _ = stop_tx.try_send(());
                }
            });
        }

        // The local endpoint owns outbound delivery independently of individual peer attachments.
        let link_stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let link_task = {
            let listener = crate::rc::local::listen()?;
            d.lock().await.online = true;
            let local_events = _link_ev_tx.clone();
            let controller = controller.clone();
            let admission = admission.clone();
            tokio::spawn(async move {
                if let Err(error) = crate::rc::endpoint::serve(
                    listener,
                    _out_rx,
                    local_events,
                    controller,
                    identity,
                    admission,
                )
                .await
                {
                    eprintln!("agitd: local transport stopped: {error:#}");
                }
            })
        };

        // Whether the outbound queue is backed up. Only used to collapse that warning into one.
        let mut outbound_full = false;
        // **Which terminals** have already received a gap notice during this round of
        // congestion.
        //
        // Tracked per terminal rather than as one global flag: a single link can have
        // several terminals open, and a global flag gives the notice to the first one
        // while the rest silently lose bytes — which is exactly the situation this notice
        // exists to kill. Once per terminal is enough; saying it on every dropped frame
        // only floods the screen.
        let mut term_gap_noted: std::collections::BTreeSet<String> = Default::default();
        // Viewer RPCs may wait through three bounded phases and, after a TAKEN
        // timeout, retain a per-session guard until the executor really closes.
        // Keep every such worker in this run-local set: shutdown must be able to
        // cancel QUEUED tickets before dropping the live session queues, and no
        // worker may outlive the state coordinator it needs for finalization.
        let mut session_rpc_tasks = tokio::task::JoinSet::new();
        let (session_rpc_stop_tx, _) = tokio::sync::watch::channel(false);
        let mut watch_rpc_queue = watch_rpc::WatchRpcQueue::default();
        let mut stopping = false;
        let mut shutdown_projection_tail: Option<ShutdownProjectionTail> = None;
        let shutdown_deadline = tokio::time::sleep(SESSION_RPC_SHUTDOWN_GRACE);
        tokio::pin!(shutdown_deadline);

        // The main pump.
        //
        // **Only events enter the journal**: events are numbered and buffered for replay
        // after a disconnection. A reply to an instruction is not numbered — it answers
        // one request, replaying it is meaningless, and it punches a hole in the stream.
        loop {
            if shutdown_projection_tail.is_some_and(ShutdownProjectionTail::complete) {
                break;
            }
            tokio::select! {
                Some(f) = frames_rx.recv(), if shutdown_projection_tail.is_none_or(|tail| tail.frames > 0) => {
                    if let Some(tail) = shutdown_projection_tail.as_mut() {
                        tail.took_frame();
                    }
                    // Only an **unnumbered** event enters the journal to take a seq. A frame that
                    // comes back carrying a seq is a copy `session.subscribe` replayed from the
                    // ring — journaling it again renumbers it and re-enters the ring, and the
                    // stream's seq numbering is doubled and scrambled from then on.
                    let out = if needs_session_projection(&f) {
                        let mut g = d.lock().await;
                        let Some(frame) = g.project_session_frame(f) else {
                            continue;
                        };
                        frame
                    } else {
                        f
                    };
                    // **Do not block here.**
                    //
                    // Only `Link::run_once` drains the outbound queue; once the hub restarts (one
                    // deployment is enough), the reconnect task backs off to `link::BACKOFF_MAX_MS`
                    // between attempts, and over that stretch nobody drains it.
                    // A session that is streaming emits one frame per token and fills the queue
                    // within minutes — then the main loop stops on this `.await`, and the moment it
                    // stops **every other branch of the select goes dead**: link events are not
                    // received (so the reconnect handshake's `Connected` is never handled, a
                    // self-inflicted deadlock), notes are not received, the watermark is not
                    // persisted, and `agit rc stop` and Ctrl-C stop answering.
                    //
                    // So enqueueing never waits: the reply lane is unbounded (a loss there cannot
                    // be recovered), the event lane is bounded and drops when full (those frames
                    // are already in the journal's ring; a viewer resubscribes to catch up).
                    let ordered_terminal_exit = OrderedTail::terminal_exit(&out);
                    let must_order = OrderedTail::must_order(&out);
                    match send_live_frame(&out_tx, &ordered_tail, out) {
                        crate::rc::outbound::Sent::Queued => {
                        }
                        crate::rc::outbound::Sent::DroppedReplayable(dropped) => {
                            let stream = dropped.stream.clone().unwrap_or_default();
                            let dropped_method = dropped.method().to_string();
                            let terminal_id = dropped
                                .params
                                .as_ref()
                                .and_then(|p| p.get("terminal_id"))
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.is_empty())
                                .map(str::to_string);
                            if !ordered_terminal_exit && !outbound_full {
                                outbound_full = true;
                                eprintln!(
                                    "agitd: the hub link is backed up; dropping live frames until it drains (session transcripts are unaffected — viewers resubscribe to catch up)"
                                );
                            }
                            // **A loss of terminal bytes has to be said out loud.**
                            //
                            // A lost session event is recoverable: it sits in the
                            // journal's ring and one `session.subscribe` from a viewer
                            // brings it back. Terminal bytes have no such path — a PTY is
                            // a stream, not a record, and the ring does not keep it (see
                            // `Journal::Stream::retained`). So this loss is permanent, and
                            // the screen is merely **missing a stretch**, with nothing to
                            // show that anything went missing.
                            //
                            // Add a visible notice. It travels the tail that neither drops
                            // frames nor breaks stream order.
                            //
                            // The notice must hang on **the terminal whose frame was
                            // dropped**: the web interface dispatches bytes to a specific
                            // pane by `terminal_id`, and an empty id matches nobody, which
                            // is the same as not sending it. The id comes from the dropped
                            // frame — it carries one already.
                            // Neither kind of frame may be dropped, **nor may either cut
                            // the line**: they carry `(stream, seq)`, and getting ahead of
                            // a smaller seq on the same stream fabricates a gap for the
                            // hub — the web interface would see "the terminal has exited"
                            // first and the last lines before that exit second.
                            //
                            // So hand it to the tail task: it waits at the **back** of the
                            // event queue for a slot (`send_ordered`), and the order it
                            // comes out in is the order it went in here. The main loop
                            // does not wait; it puts the frame down and moves on.
                            let ordered = if must_order {
                                Some(*dropped)
                            } else if let Some(term_id) = terminal_id.as_deref()
                                && stream.starts_with("term:")
                                && term_gap_noted.insert(term_id.to_string())
                            {
                                Some(gap_notice(&dropped, term_id))
                            } else {
                                None
                            };
                            if dropped_method == method::TERMINAL_EXITED
                                && let Some(term_id) = terminal_id.as_deref()
                            {
                                // Ids are not reused, and after the exit frame the same
                                // terminal produces no more output; dropping the record
                                // right away keeps the set bounded by the number of live
                                // terminals even under continuous traffic where the queue
                                // never fully drains.
                                term_gap_noted.remove(term_id);
                            }
                            if let Some(f) = ordered {
                                let reserved = OrderedTail::reserved_by_reader(&f);
                                if ordered_tail.enqueue(f, reserved).is_err() {
                                // The tail consumer is gone; carrying on would silently
                                // drop "the terminal has exited", so shut the daemon down
                                // and let the supervisor restart it explicitly.
                                    eprintln!(
                                        "agitd: the outbound terminal tail stopped before accepting a frame for {stream}"
                                    );
                                    begin_daemon_stop(
                                        &mut stopping,
                                        &link_stopping,
                                        &session_rpc_stop_tx,
                                        shutdown_deadline.as_mut(),
                                    );
                                }
                            }
                        }
                        crate::rc::outbound::Sent::Closed => {
                            begin_daemon_stop(
                                &mut stopping,
                                &link_stopping,
                                &session_rpc_stop_tx,
                                shutdown_deadline.as_mut(),
                            );
                        }
                    }
                }
                Some(stream) = tail_ack_rx.recv() => {
                    ordered_tail.acknowledge(&stream);
                }
                Some(ev) = link_ev_rx.recv() => {
                    #[cfg(feature = "cli")]
                    if let link::LinkEvent::Frame { epoch, frame } = &ev
                        && frame.is_request() && connection_epoch_is_current(&settlement_tx, *epoch) {
                        crate::telemetry::rc_request(frame.method());
                    }
                    if !stopping { match ev {
                        link::LinkEvent::Frame { epoch, frame }
                            if matches!(frame.method(), method::SESSION_START | method::SESSION_RESUME) =>
                        {
                            if !connection_epoch_is_current(&settlement_tx, epoch) { continue; }
                            let Some(id) = frame.id.clone() else { continue };
                            if session_rpc_tasks.len() >= 32 {
                                let _ = out_tx.send(Frame::error_response(id, RpcError::new(
                                    ErrorCode::SessionBusy, "session opening is busy; retry shortly")));
                                continue;
                            }
                            let prepared = {
                                let mut g = d.lock().await;
                                if !connection_epoch_is_current(&g.settlement, epoch) { continue; }
                                g.prepare_opening(&frame, &frames_tx)
                            };
                            match prepared {
                                Ok(opening) => { session_rpc_tasks.spawn(opening.serve(
                                    d.clone(), out_tx.clone(), id, session_rpc_stop_tx.subscribe(),
                                )); }
                                Err(error) => { let _ = out_tx.send(Frame::error_response(id, error)); }
                            }
                        }

                        link::LinkEvent::Frame { epoch, frame }
                            if matches!(frame.method(), method::SESSION_WATCH | method::SESSION_UNWATCH) =>
                        {
                            if !connection_epoch_is_current(&settlement_tx, epoch) { continue; }
                            let Some(id) = frame.id.clone() else { continue };
                            if session_rpc_tasks.len() >= 32 {
                                let _ = out_tx.send(Frame::error_response(id, RpcError::new(
                                    ErrorCode::SessionBusy, "session opening is busy; retry shortly")));
                                continue;
                            }
                            match watch_rpc_queue.reserve(&frame) {
                                Ok(ticket) => {
                                    session_rpc_tasks.spawn(ticket.serve(d.clone(), out_tx.clone(),
                                        frames_tx.clone(), *frame, epoch, session_rpc_stop_tx.subscribe()));
                                }
                                Err(error) => { let _ = out_tx.send(Frame::error_response(id, error)); }
                            }
                        }
                        link::LinkEvent::Frame { epoch, frame }
                            if frame.method() == method::SESSION_LIST =>
                        {
                            if !connection_epoch_is_current(&settlement_tx, epoch) { continue; }
                            let Some(id) = frame.id.clone() else { continue };
                            if session_rpc_tasks.len() >= 32 {
                                let _ = out_tx.send(Frame::error_response(id, RpcError::new(ErrorCode::SessionBusy, "session discovery is busy; retry shortly")));
                                continue;
                            }
                            let prepared = {
                                let g = d.lock().await;
                                if !connection_epoch_is_current(&g.settlement, epoch) { continue; }
                                g.prepare_session_list(&frame)
                            };
                            match prepared {
                                Ok(snapshot) => {
                                    let d = d.clone();
                                    let out = out_tx.clone();
                                    let roots = snapshot.roots.clone();
                                    let include_local = frame.params_as::<SessionList>().is_ok_and(|params| params.include_local);
                                    session_rpc_tasks.spawn(async move {
                                        let scanned = tokio::task::spawn_blocking(move || {
                                            if include_local { snapshot.scan(LocalSessionScan::Listing) } else { vec![] }
                                        }).await;
                                        let mut g = d.lock().await;
                                        if !connection_epoch_is_current(&g.settlement, epoch) { return; }
                                        let result = scanned.map_err(|_| RpcError::new(ErrorCode::Internal, "session discovery worker failed"))
                                            .and_then(|local| g.finish_session_list(&frame, &roots, local));
                                        let response = match result {
                                            Ok(value) => Frame::response(id, value),
                                            Err(error) => Frame::error_response(id, error),
                                        };
                                        let _ = out.send(response);
                                    });
                                }
                                Err(error) => { let _ = out_tx.send(Frame::error_response(id, error)); }
                            }
                        }
                        link::LinkEvent::Frame { epoch, frame }
                            if super::session_metadata::is_session_metadata(frame.method()) =>
                        {
                            if !connection_epoch_is_current(&settlement_tx, epoch) { continue; }
                            let Some(id) = frame.id.clone() else { continue };
                            if session_rpc_tasks.len() >= 32 {
                                let _ = out_tx.send(Frame::error_response(id, RpcError::new(ErrorCode::SessionBusy, "session metadata is busy; retry shortly")));
                                continue;
                            }
                            let prepared = {
                                let state = d.lock().await;
                                if !connection_epoch_is_current(&state.settlement, epoch) { continue; }
                                state.prepare_session_metadata(&frame)
                            };
                            match prepared {
                                Ok(prepared) => {
                                    let daemon = d.clone();
                                    let outbound = out_tx.clone();
                                    let mut stop = session_rpc_stop_tx.subscribe();
                                    session_rpc_tasks.spawn(async move {
                                        let result = prepared.execute(daemon, &mut stop).await;
                                        let frame = match result {
                                            Ok(value) => Frame::response(id, value),
                                            Err(error) => Frame::error_response(id, error),
                                        };
                                        let _ = outbound.send(frame);
                                    });
                                }
                                Err(error) => { let _ = out_tx.send(Frame::error_response(id, error)); }
                            }
                        }
                        link::LinkEvent::Frame { epoch, frame }
                            if frame.method() == method::SESSION_ENQUEUE =>
                        {
                            if !connection_epoch_is_current(&settlement_tx, epoch) { continue; }
                            let Some(id) = frame.id.clone() else { continue };
                            if session_rpc_tasks.len() >= 32 {
                                let _ = out_tx.send(Frame::error_response(id, RpcError::new(
                                    ErrorCode::SessionBusy, "native inbox is busy; retry this client message id")));
                                continue;
                            }
                            let snapshot = {
                                let state = d.lock().await;
                                state.prepare_session_list(&frame)
                            };
                            match snapshot {
                                Ok(snapshot) => {
                                    let out = out_tx.clone();
                                    let daemon = d.clone();
                                    session_rpc_tasks.spawn(async move {
                                        let result = async {
                                            let request: crate::rc::native_inbox::Request = frame.params_as()?;
                                            request.validate().map_err(|error| RpcError::new(ErrorCode::MalformedFrame, error.to_string()))?;
                                            let local = tokio::task::spawn_blocking(move || snapshot.scan(LocalSessionScan::Locate)
                                                .into_iter().find(|local| local.runtime_session_id == request.session_id))
                                                .await.map_err(|_| RpcError::new(ErrorCode::Internal, "native inbox discovery failed"))?;
                                            let prepared = {
                                                let state = daemon.lock().await;
                                                if !connection_epoch_is_current(&state.settlement, epoch) {
                                                    return Err(RpcError::new(ErrorCode::SessionBusy, "connection changed before native delivery"));
                                                }
                                                state.prepare_native_inbox(&frame, local)?
                                            };
                                            prepared.deliver().await.map_err(|error| RpcError::new(ErrorCode::Internal, error.to_string()))
                                        }.await;
                                        let response = match result {
                                            Ok(value) => Frame::response(id, value),
                                            Err(error) => Frame::error_response(id, error),
                                        };
                                        let _ = out.send(response);
                                    });
                                }
                                Err(error) => { let _ = out_tx.send(Frame::error_response(id, error)); }
                            }
                        }
                        link::LinkEvent::Frame { epoch, frame }
                            if is_queued_session_rpc(frame.method()) =>
                        {
                            // Queueing a command and waiting for its receipt can
                            // span three SESSION_REPLY_TIMEOUT windows. Prepare
                            // and fence it under the daemon mutex, then carry
                            // only the per-session guard across those waits.
                            if !connection_epoch_is_current(&settlement_tx, epoch) {
                                continue;
                            }
                            let frame = *frame;
                            let Some(id) = frame.id.clone() else { continue };
                            let prepared = {
                                let mut g = d.lock().await;
                                // The socket can turn over while this task was
                                // waiting briefly for the state lock. An old
                                // request must never borrow the new socket's
                                // caller/feature authority.
                                if !connection_epoch_is_current(&g.settlement, epoch) {
                                    continue;
                                }
                                g.prepare_session_rpc_or_wait(&frame)
                            };
                            match prepared {
                                Ok(SessionRpcPreparation::Ready(prepared)) => {
                                    let d = d.clone();
                                    let outbound = out_tx.clone();
                                    session_rpc_tasks.spawn((*prepared).serve(
                                        d,
                                        outbound,
                                        id,
                                        session_rpc_stop_tx.subscribe(),
                                    ));
                                }
                                Ok(SessionRpcPreparation::AwaitingDurableGuardRow(lease)) => {
                                    // The lease is per-session. Holding it keeps
                                    // later instructions ordered, while the
                                    // waiter releases the daemon mutex between
                                    // polls so `SessionNote::Bound` can land.
                                    let d = d.clone();
                                    let outbound = out_tx.clone();
                                    session_rpc_tasks.spawn(lease.serve_when_bound(
                                        SessionRpcBoundWait {
                                        daemon: d,
                                        outbound,
                                        id,
                                        frame,
                                        connection_epoch: epoch,
                                        stop: session_rpc_stop_tx.subscribe(),
                                        },
                                    ));
                                }
                                Err(error) => {
                                    // Preparation is synchronous and has not
                                    // queued a side effect. The request id is
                                    // still entitled to exactly one reply on
                                    // the reconnect-stable reply lane.
                                    let _ = out_tx.send(Frame::error_response(id, error));
                                }
                            }
                        }
                        ev => {
                            let mut g = d.lock().await;
                            g.on_link_event(ev, &frames_tx).await;
                        }
                    } }
                }
                Some(note) = notes_rx.recv(), if shutdown_projection_tail.is_none_or(|tail| tail.notes > 0) => {
                    if let Some(tail) = shutdown_projection_tail.as_mut() {
                        tail.took_note();
                    }
                    let mut g = d.lock().await;
                    g.on_session_note(note);
                }
                Some(result) = session_rpc_tasks.join_next(), if !session_rpc_tasks.is_empty() => {
                    if let Err(error) = result {
                        eprintln!("agitd: a session RPC worker failed: {error}");
                    }
                }
                _ = flush.tick() => {
                    // A round of congestion is over only when the event FIFO is **really empty**
                    // and the tail has no frame still waiting to be queued. A slow link that takes
                    // one frame at a time oscillates between full and full-1; a successful
                    // `try_send` must not be read as recovery, or the gap notice floods the screen.
                    if outbound_full && out_tx.events_drained() && ordered_tail.is_empty() {
                        outbound_full = false;
                        term_gap_noted.clear();
                    }
                    let mut g = d.lock().await;
                    // Reap, in passing, read-only watches that have been quiet too long. This
                    // tick is already firing, and the test must run under the same lock as
                    // "add a viewer" — see `reap_idle_watches`.
                    g.reap_idle_watches();
                    g.reconcile_finished_sessions(&frames_tx);
                    g.journal.flush();
                    // Grants are changed by **a person on this machine** with `agit rc grant`,
                    // editing that file on disk; the daemon gets no notification. So reread it on
                    // every heartbeat — the file is small, and without this step "a grant takes
                    // effect immediately" is an empty phrase: the in-memory copy is frozen at the
                    // moment the daemon started, and typing the command does nothing.
                    g.reload_grants();
                }
                Some(request) = safe_stop_rx.recv(), if !stopping => {
                    let prepared = {
                        let state = d.lock().await;
                        state.prepare_safe_stop(&request, &admission, !session_rpc_tasks.is_empty(), || !controller.list().is_empty())
                    };
                    match prepared {
                        Err(reply) => { let _ = request.reply.send(reply.into()); }
                        Ok(frozen) => {
                            if request.reply.send(control::Reply::Stopping).is_ok() {
                                frozen.commit();
                                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), request.written).await;
                                begin_daemon_stop(&mut stopping, &link_stopping, &session_rpc_stop_tx, shutdown_deadline.as_mut());
                            }
                        }
                    }
                }
                _ = stop_rx.recv(), if !stopping => {
                    begin_daemon_stop(
                        &mut stopping,
                        &link_stopping,
                        &session_rpc_stop_tx,
                        shutdown_deadline.as_mut(),
                    );
                }
                _ = &mut shutdown_deadline, if stopping && !session_rpc_tasks.is_empty() => {
                    finish_session_rpcs_at_deadline(
                        &d,
                        &mut session_rpc_tasks,
                    ).await;
                }
            }
            if stopping && session_rpc_tasks.is_empty() {
                // Every admitted RPC's supervisor sends its permission/danger
                // projection before closing the receipt. JoinSet completion is
                // therefore the producer barrier; capture and drain exactly
                // the prefix already queued at this instant. Waiting for Empty
                // is not a valid boundary because live sessions can continue
                // to emit unrelated transcript frames until shutdown.
                if shutdown_projection_tail.is_none() {
                    shutdown_projection_tail =
                        Some(ShutdownProjectionTail::capture(&frames_rx, &notes_rx));
                }
            }
        }

        link_task.abort();

        debug_assert!(
            session_rpc_tasks.is_empty(),
            "shutdown projection capture requires every RPC producer to be joined"
        );

        // The tail may be stuck "waiting for a slot" — the link is gone and it will never
        // get one. Nobody drains the outbound queue on the shutdown path, so the only
        // option is to cut it, or the process is left with a task that never exits.
        tail_task.abort();
        let mut g = d.lock().await;
        // Transport is already torn down above, while the **exit settlement** that runs in
        // `shutdown()` still commits, pushes and emits `commit.settled` — that notification
        // has no consumer left, so it can never get the hub's acknowledgement. This is not a
        // bug (waiting on a notification that cannot be delivered only hangs shutdown), but
        // it means "the push succeeded" on this path is **never** the same as "the hub took
        // the notification": git's remote-tracking ref has advanced while the notification
        // stayed on this machine. So the notification side carries its own durable
        // watermark — see `supervisor::unacked_settlement_path`: the receipt persists before
        // the push and is removed only once the hub really acknowledges, and the next daemon
        // to start resends it from there. Anyone who changes settlement's "delivered" test
        // back to inferring it from git reachability loses this whole argument again.
        g.shutdown().await;
        #[cfg(windows)]
        control::clear_pidfile();
        Ok(())
    }

    fn status(&self) -> control::Status {
        control::Status {
            identity: Some(self.identity.clone()),
            pid: std::process::id(),
            hub: self.opts.hub.clone(),
            online: self.online,
            uptime_secs: self.started_at.elapsed().as_secs(),
            agit_version: env!("CARGO_PKG_VERSION").to_string(),
            sessions: self
                .sessions
                .values()
                .map(|l| control::SessionLine {
                    session_id: l.info.session_id.clone(),
                    runtime: l.info.runtime.clone(),
                    status: format!("{:?}", l.info.status).to_lowercase(),
                    last_seq: self.journal.last_seq(&l.info.session_id),
                })
                .collect(),
        }
    }

    pub(super) fn settlement_feature(&self) -> bool {
        let state = self.settlement.borrow();
        state.agent_identity_v1 || state.local_owner
    }

    pub(super) fn start_idempotency_feature(&self) -> bool {
        self.settlement.borrow().session_start_idempotency_v1
    }

    async fn on_link_event(&mut self, ev: link::LinkEvent, frames: &mpsc::Sender<Frame>) {
        match ev {
            link::LinkEvent::Frame { epoch, frame }
                if connection_epoch_is_current(&self.settlement, epoch) =>
            {
                self.on_frame(*frame, frames).await
            }
            // The daemon may be busy in one slow dispatch while the link
            // disconnects and registers a newer socket. Never let an old
            // queued instruction borrow that newer socket's feature ACK.
            link::LinkEvent::Frame { .. } => {}
        }
    }

    /// Handle one instruction relayed from a viewer.
    async fn on_frame(&mut self, f: Frame, frames: &mpsc::Sender<Frame>) {
        let Some(id) = f.id.clone() else { return };
        let reply = self.dispatch(&f, frames).await;
        let out = match reply {
            Ok(v) => Frame::response(id, v),
            Err(e) => Frame::error_response(id, e),
        };
        // The replay belongs to this response, never to the shared event fanout.
        let deferred = std::mem::take(&mut self.deferred);
        let slot = self.deferred_slot.take();
        if let Some(outbound) = self.outbound.clone() {
            if let Some(slot) = slot {
                let _ = outbound.send_replay_response(out, deferred, slot);
            } else {
                debug_assert!(deferred.is_empty());
                let _ = outbound.send(out);
            }
            return;
        }

        // The link is not up yet (only during the instant of startup): fall back to the
        // main loop for forwarding. Do not send the reply and drop `deferred`; those
        // frames are exactly what this subscribe promised to fill in.
        let frames = frames.clone();
        tokio::spawn(async move {
            let _slot = slot;
            if frames.send(out).await.is_err() {
                return;
            }
            for frame in deferred {
                if frames.send(frame).await.is_err() {
                    return;
                }
            }
        });
    }

    pub(super) async fn shutdown(&mut self) {
        let mut terminal_cleanup = Vec::with_capacity(self.terminals.len());
        for (_, t) in self.terminals.drain() {
            terminal_cleanup.push(t.term.cleanup_handle());
            t.term.kill();
        }
        // Every terminal is signalled together first, then their individual reaper /
        // force-kill threads are waited for on the blocking pool. A Condvar must not be
        // awaited on a Tokio worker, and terminals must not be killed and waited for one
        // at a time (that chains the grace period per terminal). The whole batch shares
        // the single `TERMINAL_CLEANUP_GRACE` bound.
        if !terminal_cleanup.is_empty() {
            let _ = tokio::task::spawn_blocking(move || {
                let deadline = std::time::Instant::now() + TERMINAL_CLEANUP_GRACE;
                for cleanup in terminal_cleanup {
                    let left = deadline.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() || !cleanup.wait_timeout(left) {
                        break;
                    }
                }
            })
            .await;
        }
        for (_, w) in self.watches.drain() {
            w.handle.abort();
        }
        // **Notify everyone first, then wait for them together, and keep the whole stretch
        // under one bound.**
        //
        // Notifying and waiting one session at a time makes n sessions wait n grace
        // periods serially; broadcasting first lets them finish up in parallel.
        //
        // The notification uses `try_send` and not `send().await`: that queue holds a
        // fixed number of slots, a session **does not consume** ordinary instructions
        // while it is settling, and the slots may be held by RPCs that already timed out
        // and withdrew. `await` on a full queue is indefinite: blocking on one session
        // eats the entire grace period and the sessions behind it never even get
        // notified. Failing to push it in is fine: the channel's **sending end is dropped
        // here**, so `commands.recv()` on the session side immediately gets `None` and
        // takes the same finishing path as a `Shutdown`.
        let _ = tokio::time::timeout(FLEET_EXIT_GRACE, async {
            let mut tasks = vec![];
            for (_, l) in self.sessions.drain() {
                let _ = l.tx.try_send(Command::Shutdown);
                drop(l.tx);
                tasks.push(l.task);
            }
            // Wait for them to really finish. Signalling and exiting leaves the harness's
            // teardown (SIGTERM → `SHUTDOWN_GRACE_MS` → SIGKILL) no time to run, and those
            // child processes become orphans — while they hold file locks on the user's repo.
            for t in tasks {
                let _ = t.await;
            }
        })
        .await;
        // **Settlement authority is revoked only after the whole fleet has exited cleanly.**
        //
        // Revoking it before the sessions receive `Shutdown` (by putting it in
        // `begin_daemon_stop`, say) makes the first line of `settle_on_exit` →
        // `settle_and_push` fail to take the lease and return: every `agit rc stop` exit
        // settlement does nothing. Placed here, "no longer authorized to settle" really
        // does coincide in time with "no session is settling any more".
        let revoked_epoch = self.settlement.borrow().epoch.wrapping_add(1);
        set_connection_features(&self.settlement, revoked_epoch, false, false);
        self.journal.flush();
    }
}
