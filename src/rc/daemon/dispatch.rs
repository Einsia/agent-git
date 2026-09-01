use super::*;

/// How long a newly installed tail yields to the `session.watch` response before it starts
/// sending replay frames.
///
/// The hub registers "which workspace this stream belongs to" only on receiving that response;
/// frames that arrive before the registration are fanned out to an empty channel. See
/// [`install_watch`].
const WATCH_RESPONSE_HEADSTART: std::time::Duration = std::time::Duration::from_millis(500);

impl Daemon {
    pub(super) async fn dispatch(
        &mut self,
        f: &Frame,
        frames: &mpsc::Sender<Frame>,
    ) -> Result<serde_json::Value, RpcError> {
        // Every arm below reads `p.workspace_id` safely because of this line: past it, the value
        // in params is proven equal to the one the hub stamped. See [`caller_scope`].
        let caller = caller_scope(f)?;
        // Tenant first, then role. They are split because they answer two questions — "is the
        // workspace you named your own" and "are you allowed to do this in that workspace".
        require_role(&caller, f.method())?;
        match f.method() {
            method::WORKSPACE_LIST => {
                // Report **only the caller's own** workspace. The whole mirror holds every
                // workspace this machine serves along with the paths each one binds — handing
                // that to a member of A tells them who else this machine works for and which
                // directory holds someone else's code. The hub defines workspaces, so the web
                // interface never has to discover another workspace from the machine side.
                let mine: Vec<_> = self
                    .mirror
                    .to_local()
                    .into_iter()
                    .filter(|w| w.workspace_id == caller.workspace_id)
                    .collect();
                Ok(serde_json::to_value(WorkspaceListResult { workspaces: mine }).unwrap())
            }

            method::FS_READ_DIRECTORY => {
                let p: FsReadDirectory = f.params_as()?;
                let target = if p.path.trim().is_empty() {
                    std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .unwrap_or_default()
                } else {
                    PathBuf::from(&p.path)
                };
                // Scoped by ownership, not by allowlist: this runs before any
                // project exists, so it is how the folder picker works at all.
                let dir = policy::require_dir_under_home(&target).map_err(|e| {
                    RpcError::new(ErrorCode::PathNotAllowed, e.to_string())
                        .with_hint("the picker only browses inside your home directory")
                })?;
                let mut entries = vec![];
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().to_string();
                        if name.starts_with('.') {
                            continue;
                        }
                        let is_dir = e.path().is_dir();
                        entries.push(crate::protocol::DirEntry {
                            is_git_repo: is_dir && e.path().join(".git").exists(),
                            name,
                            is_dir,
                        });
                    }
                }
                entries.sort_by(|a, b| {
                    (!a.is_dir, a.name.to_lowercase()).cmp(&(!b.is_dir, b.name.to_lowercase()))
                });
                Ok(serde_json::to_value(FsReadDirectoryResult {
                    path: dir.to_string_lossy().to_string(),
                    entries,
                })
                .unwrap())
            }

            method::PROJECT_BIND => {
                let p: ProjectBind = f.params_as()?;
                let path = PathBuf::from(&p.local_path);
                // The test for binding is **deliberately different** from the file picker's:
                // projects live under /srv, /opt, /workspace, and confining them to $HOME would
                // rule out half the real repos. What this guards is "exists, is a directory, is
                // not a system root".
                let dir = self
                    .mirror
                    .bind(&p.workspace_id, &p.project_id, &path)
                    .map_err(|e| {
                    RpcError::new(ErrorCode::PathNotAllowed, e.to_string()).with_hint(
                        "this folder becomes the workspace's allowlist, so it cannot be a system root",
                    )
                })?;
                let _ = self.mirror.save();
                self.refresh_confinement();
                Ok(serde_json::to_value(ProjectBindResult {
                    project_id: p.project_id,
                    git_origin: crate::rc::mirror::git_origin(&dir),
                    local_path: dir.to_string_lossy().to_string(),
                })
                .unwrap())
            }

            method::PROJECT_UNBIND => {
                let p: crate::protocol::ProjectUnbind = f.params_as()?;
                // The protocol constant, the params type and `Mirror::unbind` are all in place;
                // without this arm an owner unbinding a folder from the web interface gets back
                // only `UnknownMethod`, and that root stays in the operator's pass until the next
                // reconnect rebuilds the whole table through `Mirror::adopt`.
                //
                // With the test inverted this is not "slightly stale": the allowlist **is** the
                // entire basis for containment, and a root that should have disappeared is a pass
                // that stays valid.
                self.mirror.unbind(&p.workspace_id, &p.project_id);
                let _ = self.mirror.save();
                self.refresh_confinement();
                Ok(serde_json::json!({}))
            }

            method::FS_READ_FILE => {
                let p: FsReadFile = f.params_as()?;
                let roots = self.mirror.roots(&p.workspace_id);
                let path =
                    policy::require_within(std::path::Path::new(&p.path), &roots).map_err(|e| {
                        RpcError::new(ErrorCode::PathNotAllowed, e.to_string()).with_hint(
                            "the preview can only open files inside this workspace's bound folders",
                        )
                    })?;
                Ok(serde_json::to_value(read_preview(&path, p.offset)?).unwrap())
            }

            method::TERMINAL_OPEN => {
                let p: TerminalOpen = f.params_as()?;
                // While a gap or exit waits on the event FIFO, the terminals already open fill
                // the structural memory budget of that path. Allowing the open/exit loop on top
                // of that manufactures unbounded "must never be dropped" final states while the
                // link is down; admission recovers on its own once every tail is queued.
                if terminal_delivery_blocked(&self.terminal_delivery_blockers) {
                    return Err(RpcError::new(
                        ErrorCode::QuotaExceeded,
                        "terminal delivery is backed up on this machine",
                    )
                    .with_hint("retry after the hub link drains pending terminal state"));
                }
                // **The number of terminals open at once on one machine is capped.**
                //
                // Each terminal is a real PTY plus a shell process plus a read loop, and what
                // opens it is a button in the web interface: with no cap, a script starts shells on
                // someone else's machine as fast as it can loop. This is not hypothetical —
                // `terminal.open` runs on ordinary operator permission, and the viewer's loop
                // rate is the only bound on the local process count.
                //
                // A second layer: while the link is backed up, each terminal puts at most two
                // frames into the in-order tail (one gap marker plus one end). With no cap on
                // terminals, pausing new admission still leaves the already-pending volume
                // without a hard bound.
                if self.terminals.len() >= MAX_TERMINALS {
                    return Err(RpcError::new(
                        ErrorCode::QuotaExceeded,
                        format!("this machine already has {MAX_TERMINALS} terminals open"),
                    )
                    .with_hint("close one before opening another"));
                }
                let roots = self.mirror.roots(&p.workspace_id);
                let cwd = match &p.project_id {
                    Some(pid) => self.mirror.project_path(&p.workspace_id, pid),
                    None => roots.first().cloned(),
                }
                .ok_or_else(|| {
                    RpcError::new(
                        ErrorCode::PathNotAllowed,
                        "this workspace has no bound folder to open a terminal in",
                    )
                    .with_hint("bind a project folder first")
                })?;
                let cwd = policy::require_within(&cwd, &roots)
                    .map_err(|e| RpcError::new(ErrorCode::PathNotAllowed, e.to_string()))?;

                // Terminal bytes and session events share one return path (the same outbound
                // queue, the same WSS). **The backfill half does not hold for them** — terminal
                // streams never enter the replay buffer and `session.subscribe` does not accept
                // them, for the reason at the top of `protocol`. What is lost is lost, so a
                // backed-up link writes a visible marker into that terminal.
                let tx = match &self.term_tx {
                    Some(t) => t.clone(),
                    None => {
                        let (tx, mut rx) = mpsc::channel::<TerminalEvent>(2048);
                        let frames = frames.clone();
                        let notes = self.notes.clone();
                        let terminal_delivery_blockers = self.terminal_delivery_blockers.clone();
                        tokio::spawn(async move {
                            while let Some(ev) = rx.recv().await {
                                let (mut f, ws) = match ev {
                                    TerminalEvent::Output {
                                        id,
                                        workspace_id,
                                        data,
                                    } => (terminal_output_frame(id, data), workspace_id),
                                    TerminalEvent::Exited {
                                        id,
                                        workspace_id,
                                        code,
                                    } => (
                                        {
                                            // **Take the delivery slot before releasing
                                            // the active slot.**
                                            //
                                            // The note removes the terminal from
                                            // `self.terminals`; removing it first and only
                                            // then waiting for the exit frame to reach the
                                            // main pump leaves a window where a broken link
                                            // admits repeated open/exit and manufactures
                                            // unbounded final state. This count is released
                                            // by the tail ack only once the exit is really
                                            // queued into the outbound FIFO.
                                            OrderedTail::reserve_terminal_exit(
                                                &terminal_delivery_blockers,
                                            );
                                            // The shell exited on its own: drop that row,
                                            // or `TermLive` keeps a PTY master fd and an
                                            // unreaped child process forever.
                                            let _ = notes
                                                .send(SessionNote::TerminalExited {
                                                    terminal_id: id.clone(),
                                                })
                                                .await;
                                            let f = terminal_exited_frame(id, code);
                                            // **The final state always goes through the
                                            // in-order tail.**
                                            //
                                            // "this terminal is gone" must not be dropped:
                                            // it is a state transition, not part of the
                                            // stream. Drop it and the panel on the web
                                            // interface stays open waiting for an end that
                                            // never comes, while the gap marker calls it
                                            // "output was cut short" — saying the opposite
                                            // of what happened.
                                            //
                                            // Marking it `reliable` onto the response
                                            // queue is wrong: that queue has higher
                                            // priority and overtakes output from the same
                                            // terminal still sitting in the event queue —
                                            // the end arrives first, then the bytes that
                                            // belong before it. A plain `try_send` can drop
                                            // the final state outright.
                                            //
                                            // So the main pump hands every exit to the
                                            // single-consumer tail: it waits for capacity
                                            // at the back of the event FIFO, and the stream
                                            // seal holds off latecomers in the handoff
                                            // window — nothing is dropped and nothing jumps
                                            // the queue.
                                            f
                                        },
                                        workspace_id,
                                    ),
                                };
                                // A terminal stream hangs off **that workspace**, not off
                                // any session.
                                //
                                // Hard-coding a literal here (`format!("term:{}",
                                // "workspace")`) puts every workspace's terminals on this
                                // machine onto one stream, the hub resolves its workspace to
                                // the empty string (`terminal.open` is not on
                                // `remember_workspace`'s list), and the bytes fan out to a
                                // channel nobody subscribes to: the terminal panel on the web
                                // interface is black from start to finish, while
                                // `terminal.input` is an ordinary request/response relay and
                                // looks like "typing works" — which makes this harder to
                                // notice.
                                f.stream = Some(format!("term:{ws}"));
                                if frames.send(f).await.is_err() {
                                    break;
                                }
                            }
                        });
                        self.term_tx = Some(tx.clone());
                        tx
                    }
                };

                let id = format!("t-{}", uuid::Uuid::new_v4().simple());
                let t = Terminal::open(
                    id.clone(),
                    caller.workspace_id.clone(),
                    &cwd,
                    p.cols,
                    p.rows,
                    tx,
                )
                .map_err(|e| RpcError::new(ErrorCode::Internal, e.to_string()))?;
                let res = TerminalOpenResult {
                    terminal_id: id.clone(),
                    cwd: t.cwd.clone(),
                    shell: t.shell.clone(),
                };
                self.terminals.insert(
                    id,
                    TermLive {
                        workspace_id: caller.workspace_id.clone(),
                        term: t,
                    },
                );
                Ok(serde_json::to_value(res).unwrap())
            }

            method::TERMINAL_INPUT => {
                let p: TerminalInput = f.params_as()?;
                let t = self.terminal_owned_by(&p.terminal_id, &caller)?;
                t.write(&p.data)
                    .map_err(|e| RpcError::new(ErrorCode::Internal, e.to_string()))?;
                Ok(serde_json::json!({}))
            }

            method::TERMINAL_RESIZE => {
                let p: TerminalResize = f.params_as()?;
                let t = self.terminal_owned_by(&p.terminal_id, &caller)?;
                let _ = t.resize(p.cols, p.rows);
                Ok(serde_json::json!({}))
            }

            method::TERMINAL_CLOSE => {
                let p: TerminalClose = f.params_as()?;
                // Check ownership before removing: finding out after `remove` that it was not
                // yours leaves that terminal already gone.
                self.terminal_owned_by(&p.terminal_id, &caller)?;
                if let Some(t) = self.terminals.remove(&p.terminal_id) {
                    t.term.kill();
                }
                Ok(serde_json::json!({}))
            }

            method::SESSION_START => {
                let p: SessionStart = f.params_as()?;
                self.start_session(p, &caller, frames).await
            }

            method::SESSION_LIST => {
                let p: SessionList = f.params_as().unwrap_or(SessionList {
                    workspace_id: String::new(),
                    include_local: false,
                });
                // Report only the live sessions of **this workspace**. Listing the whole
                // `self.sessions` table hands every member of A the session ids, gists and
                // danger bits of the other workspaces this machine serves at the same time — and
                // that id is how every session verb below addresses its target.
                let live: Vec<SessionInfo> = self
                    .sessions
                    .values()
                    .filter(|l| l.info.workspace_id == caller.workspace_id)
                    .map(|l| self.stamped(l.info.clone()))
                    .collect();
                let local = if p.include_local {
                    self.local_sessions(&p.workspace_id, LocalSessionScan::Listing)
                } else {
                    vec![]
                };
                Ok(serde_json::to_value(SessionListResult {
                    sessions: live,
                    local,
                })
                .unwrap())
            }

            method::SESSION_RESUME => {
                let p: SessionResume = f.params_as()?;
                self.resume_session(p, &caller, frames).await
            }

            // Follow a session already open in someone else's terminal, read-only. It starts no
            // process and writes nothing — it only tails the transcript the harness is appending,
            // so it is immune to the data-corruption guard behind "a live session cannot be taken
            // over": a reader damages no history.
            method::SESSION_WATCH => {
                let p: SessionWatch = f.params_as()?;
                if !self.mirror.has_workspace(&p.workspace_id) {
                    return Err(RpcError::new(
                        ErrorCode::WorkspaceNotFound,
                        format!("workspace {} is not bound on this machine", p.workspace_id),
                    ));
                }
                let local = self.locate_local(&p.workspace_id, &p.session_id)?;
                let (runtime, cwd, project_id) = (local.runtime, local.cwd, local.project_id);
                let roots = self.mirror.roots(&p.workspace_id);
                let cwd = policy::require_within(&cwd, &roots)
                    .map_err(|e| RpcError::new(ErrorCode::PathNotAllowed, e.to_string()))?;
                // `Box<dyn Adapter>` is not Send, so path resolution is confined to its own
                // scope.
                let path = {
                    let Ok(adapter) = crate::adapter::get(&runtime) else {
                        return Err(RpcError::new(
                            ErrorCode::RuntimeUnavailable,
                            format!("no adapter for runtime {runtime}"),
                        ));
                    };
                    adapter.resolve(&p.session_id, Some(&cwd)).ok_or_else(|| {
                        RpcError::new(
                            ErrorCode::SessionNotFound,
                            format!("cannot locate the transcript of {}", p.session_id),
                        )
                    })?
                };
                let (start_off, from_line, total_lines, absolute_lines) =
                    tail_window(&path, WATCH_BACKFILL_LINES);
                // The stream id comes from the thread id — several people watching the same
                // session still share one stream and one run of seqs.
                let watch_id = watch_stream_id(&caller.workspace_id, &p.session_id);

                let now = chrono::Utc::now().to_rfc3339();
                let info = SessionInfo {
                    session_id: watch_id.clone(),
                    workspace_id: p.workspace_id.clone(),
                    project_id,
                    runtime: runtime.clone(),
                    agent: None,
                    branch: None,
                    status: SessionStatus::Running,
                    last_seq: 0,
                    gist: None,
                    // Watching is read-only, but this field records what this session **has
                    // done**, not what you can do now. Hard-coding false hides that warning on
                    // the web interface for a session that ran with no approval — exactly when it
                    // most needs to show.
                    // The roster keys on the logical `agit-*` id while `session.watch` receives a
                    // harness-native thread id. Looking the latter up directly always lands on
                    // "no such row" and misses the real monotonic danger bit.
                    dangerous: danger::judge(
                        &self.roster,
                        &runtime,
                        &p.session_id,
                        &p.workspace_id,
                        &cwd.to_string_lossy(),
                    )
                    .ever_dangerous(),
                    permission_mode: None,
                    created_at: now.clone(),
                    updated_at: now,
                };

                // **A row in the table does not mean that tail is still alive.**
                //
                // It may have exited on its own (the transcript is gone, or it stayed quiet past
                // `WATCH_IDLE_STOP`) while its `WatchEnded` still sits unconsumed in the notes
                // queue. Only incrementing viewers then attaches the new viewer to a dead tail —
                // not one frame arrives, and the notification right behind it removes the row, so
                // even "who is watching" is gone.
                let stale = self
                    .watches
                    .get(&watch_id)
                    .is_some_and(|w| w.handle.is_finished());
                if stale {
                    self.take_watch(&watch_id);
                }
                match self.watches.get_mut(&watch_id) {
                    // Someone is already watching (and that tail really is alive): add a
                    // subscriber rather than start a second one.
                    Some(w) => {
                        *w.viewers.entry(caller_key(&caller)).or_insert(0) += 1;
                        // Renew the lease. This runs under **the same lock** as the reaping
                        // decision (see `reap_idle_watches`), so there is no gap between "a
                        // viewer just joined" and "it is about to exit".
                        w.renew();
                    }
                    None => {
                        self.watch_generation += 1;
                        let generation = self.watch_generation;
                        // The tail only reports when it last saw activity; the daemon decides
                        // when to reap.
                        let active_at =
                            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(now_secs()));
                        let active = active_at.clone();
                        let frames = frames.clone();
                        let notes = self.notes.clone();
                        let stream = watch_id.clone();
                        let rt = runtime.clone();
                        // A read-only follow and a supervised session take the same outbound
                        // path, so they share the daemon's secret filter: loading a copy here
                        // freezes a snapshot on this stream, which keeps allowing by the old
                        // rules after `agit rc secrets reload`.
                        let secret_filter = self.secret_filter.clone();
                        let handle = tokio::spawn(async move {
                            let redactor = crate::domain::redact::Redactor::with_registered(
                                crate::domain::redact::Persona::this_machine(),
                                secret_filter,
                            );
                            // Read from the start of the window instead of reading from the
                            // beginning and discarding — the latter costs memory the size of
                            // the whole transcript.
                            let mut tailer =
                                crate::rc::tail::Tailer::at(&path, start_off, from_line);
                            // Let the `session.watch` **response** out first: the hub registers
                            // "which workspace this stream belongs to" only on that response,
                            // and replay frames that arrive before the registration fan out to
                            // an empty channel. Arriving early loses nothing — the frames are in
                            // the journal's ring, and a viewer replays them with a
                            // `session.subscribe`.
                            tokio::time::sleep(WATCH_RESPONSE_HEADSTART).await;
                            loop {
                                // Reap once the transcript is gone (the session was cleaned up,
                                // the directory was deleted).
                                if !path.exists() {
                                    break;
                                }
                                let lines = tailer.poll().unwrap_or_default();
                                if lines.is_empty() {
                                    // Quiet decides nothing here: reaping is judged by the
                                    // daemon (`reap_idle_watches`) because it has to sit
                                    // under the same lock as "add a viewer". This only
                                    // reports whether there was activity.
                                } else {
                                    // Report activity. **The daemon decides whether to
                                    // reap**, see `reap_idle_watches`.
                                    active.store(
                                        crate::rc::daemon::now_secs(),
                                        std::sync::atomic::Ordering::Release,
                                    );
                                    // A read-only follow has no session identity, and
                                    // `secret.detected` is a session-level alert: this only
                                    // guarantees the content is redacted.
                                    let (items, _registered_ids) =
                                        crate::rc::supervisor::items_from_lines(
                                            &rt, &redactor, &lines,
                                        );
                                    for item in items {
                                        let mut fr =
                                            Frame::notification(method::ITEM_COMPLETED, item);
                                        fr.stream = Some(stream.clone());
                                        if frames.send(fr).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(
                                    crate::rc::supervisor::TAIL_POLL_MS,
                                ))
                                .await;
                            }
                            // Have the daemon drop this row, or the map only ever grows.
                            let _ = notes
                                .send(SessionNote::WatchEnded {
                                    stream: stream.clone(),
                                    generation,
                                })
                                .await;
                        });
                        install_watch(
                            &mut self.journal,
                            &mut self.watches,
                            watch_id.clone(),
                            WatchLive {
                                info: info.clone(),
                                handle,
                                active: active_at,
                                viewers: [(caller_key(&caller), 1usize)].into_iter().collect(),
                                generation,
                            },
                        );
                    }
                }

                Ok(serde_json::to_value(SessionWatchResult {
                    session: self.stamped(info),
                    from_line,
                    total_lines,
                    absolute_lines,
                    read_only: true,
                })
                .unwrap())
            }

            // A viewer stops watching. The last one out shuts the tail task down — without this,
            // every session ever watched leaves this machine one more permanent file poll.
            method::SESSION_UNWATCH => {
                let p: SessionWatch = f.params_as()?;
                let watch_id = watch_stream_id(&caller.workspace_id, &p.session_id);
                if let Some(w) = self.watches.get_mut(&watch_id) {
                    // Only **your own** count comes off. See `WatchLive::viewers`.
                    let key = caller_key(&caller);
                    if let Some(n) = w.viewers.get_mut(&key) {
                        *n -= 1;
                        if *n == 0 {
                            w.viewers.remove(&key);
                        }
                    }
                    if w.viewers.is_empty()
                        && let Some(w) = self.take_watch(&watch_id)
                    {
                        w.handle.abort();
                    }
                }
                Ok(serde_json::json!({}))
            }

            method::SESSION_SUBSCRIBE => {
                let p: SessionSubscribe = f.params_as()?;
                // **Subscribing is an ownership check too.**
                //
                // It "reads" rather than "drives" — but what it reads is the full transcript of a
                // conversation on someone else's machine, and the only addressing here is the id
                // the client supplies. Without this check, a member of A who holds a session id
                // of B replays the journal's whole ring, while the `turn.*` verbs are all checked
                // by this point: the one that is missed is the only verb that actually **emits
                // content**.
                let info = self
                    .sessions
                    .get(&p.session_id)
                    .map(|l| l.info.clone())
                    // A watch stream is subscribable too — its replay frames are in the
                    // journal's ring as well. Its id carries the workspace (see
                    // `watch_stream_id`), so the ownership check below holds for it too.
                    .or_else(|| self.watches.get(&p.session_id).map(|w| w.info.clone()))
                    .filter(|i| i.workspace_id == caller.workspace_id)
                    .ok_or_else(|| no_such_session(&p.session_id))?;
                // Take the slot before taking the frames — the other order has already made the
                // copies.
                let Ok(slot) = self.replay_slots.clone().try_acquire_owned() else {
                    return Err(RpcError::new(
                        ErrorCode::SessionBusy,
                        "this machine is already sending several backfills; try again in a moment",
                    )
                    .with_hint("nothing was sent for this request — resubscribing is safe"));
                };
                let (mut replay, lowest) = self.journal.replay(&p.session_id, p.after_seq);
                // **One subscription replays at most this many frames.**
                //
                // The ring holds 8192 slots, and these frames go into the outbound queue
                // untouched (they must not be dropped). Uncapped, a viewer that resubscribes over
                // and over — a page in a reconnect loop — plants a batch in that unbounded queue
                // on every open, and on a slow link they are all still there.
                //
                // Cut the head, keep the tail: nearly all of these frames are token deltas, and
                // "keep watching" wants the **end** of this turn, not its beginning. The hub side
                // treats its in-flight ring on the same reasoning.
                const REPLAY_CAP: usize = 2000;
                let dropped = replay.len().saturating_sub(REPLAY_CAP);
                let replay = replay.split_off(dropped);
                // **Report the gap honestly.** `from_seq` means "no history below this is left
                // here" — reporting the `lowest` the ring holds after cutting a stretch off makes
                // the front end believe everything from `lowest` on is contiguous, and that hole
                // is never filled again.
                let lowest = replay.first().and_then(|f| f.seq).unwrap_or(lowest);
                // **`try_send` is wrong here.**
                //
                // The only consumer of `frames_rx` is the main select loop, and it is blocked in
                // this very dispatch (select runs the body of the branch it picked and polls no
                // other branch meanwhile) — nobody drains the channel, which is self-deadlock,
                // not a race. The ring holds 8192 slots while the channel holds 4096, so the
                // thousands of frames beyond that are dropped silently, and because they are
                // queued in ascending seq order what is dropped is the newest stretch — the tail
                // that "keep watching" needs most. The **response** to this subscription goes
                // with them. And `from_seq` claims that hole is already filled.
                //
                // Hand them to `on_frame`, which sends outside the lock under backpressure and
                // marks them undroppable: the event queue on the outbound side holds only 2000
                // slots and drops when full — while the ring can hand over 8192 frames at once.
                // Getting past the first 4096-slot channel only to be dropped at the second is
                // the other half of the same bug.
                self.deferred = replay;
                self.deferred_slot = Some(slot);
                Ok(serde_json::to_value(SessionSubscribeResult {
                    session: self.stamped(info),
                    from_seq: lowest,
                })
                .unwrap())
            }

            method::TURN_START
            | method::TURN_STEER
            | method::SESSION_SET_PERMISSION_MODE
            | method::TURN_INTERRUPT
            | method::APPROVAL_DECIDE => Err(RpcError::new(
                ErrorCode::Internal,
                "session command reached the state-locked dispatcher",
            )),

            other => Err(RpcError::new(
                ErrorCode::UnknownMethod,
                format!("unknown method {other}"),
            )
            .with_hint("upgrade agit on this machine: `agit upgrade`")),
        }
    }
}

/// Install a new read-only follow: write it into the table, **and reopen its replay ring in the
/// journal**.
///
/// The pair has to stay symmetric. Removal has one entry point, `Daemon::take_watch`, and it
/// always calls `Journal::forget` — besides releasing the ring, `forget` **permanently** turns off
/// the "does this stream keep frames" switch (`Journal::record` sets it only when the stream is
/// **created**, and after that it can only be turned off again, never back on). Every removal of a
/// watch stream is on a normal path: an explicit `session.unwatch`, idle reaping, `WatchEnded`,
/// and a reopen that finds the old task dead — the same `watch_id` getting a fresh tail is the
/// rule, not an anomaly.
///
/// So without the `resume` call, the stream takes **no frame into the ring at all** from the
/// second follow on: one hiccup in the viewer's network and `session.subscribe(after_seq)` answers
/// with an empty replay plus a `from_seq` saying "the stretch you asked for is gone", and the
/// transcript lines from the disconnected window really are lost.
/// [`WATCH_RESPONSE_HEADSTART`] bets on this same ring — it sends replay frames ahead of the hub's
/// registration precisely because "a viewer replays them with a `session.subscribe`". The session
/// side works the same way, see `journal.resume` in `resume_session`.
fn install_watch(
    journal: &mut Journal,
    watches: &mut HashMap<String, WatchLive>,
    watch_id: String,
    live: WatchLive,
) {
    journal.resume(&watch_id);
    watches.insert(watch_id, live);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the install / remove pair for a read-only follow and nothing else: every field but
    /// `journal` and `watches` is empty — this path touches none of them.
    fn watching_daemon() -> Daemon {
        let (notes, _notes_rx) = mpsc::channel(1);
        let (settlement, _) = tokio::sync::watch::channel(SettlementState::default());
        Daemon {
            deferred: vec![],
            deferred_slot: None,
            replay_slots: Arc::new(tokio::sync::Semaphore::new(REPLAY_SLOTS)),
            outbound: None,
            opts: Options {
                hub: "https://hub.invalid".into(),
                token: "test".into(),
                connection_id: None,
            },
            journal: Journal::new(),
            mirror: Mirror::default(),
            roster: Roster::default(),
            sessions: HashMap::new(),
            latest_session_generations: HashMap::new(),
            watches: HashMap::new(),
            terminals: HashMap::new(),
            terminal_delivery_blockers: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            term_tx: None,
            online: true,
            connection_id: Some("conn".into()),
            secret_filter: Default::default(),
            settlement,
            started_at: std::time::Instant::now(),
            notes,
            grants: crate::rc::grants::Grants::default(),
            watch_generation: 0,
            session_generation: 0,
            confinement: HashMap::new(),
        }
    }

    fn watching_tail(stream: &str) -> WatchLive {
        WatchLive {
            info: SessionInfo {
                session_id: stream.into(),
                workspace_id: "ws-a".into(),
                project_id: None,
                runtime: "claude-code".into(),
                agent: None,
                branch: None,
                status: SessionStatus::Running,
                last_seq: 0,
                gist: None,
                dangerous: false,
                permission_mode: None,
                created_at: String::new(),
                updated_at: String::new(),
            },
            // This test looks at the journal half; what the tail task runs does not matter.
            handle: tokio::spawn(std::future::ready(())),
            active: Arc::new(std::sync::atomic::AtomicU64::new(now_secs())),
            viewers: [("acct-1".to_string(), 1usize)].into_iter().collect(),
            generation: 1,
        }
    }

    fn watched_item() -> Frame {
        Frame::notification(method::ITEM_COMPLETED, serde_json::json!({}))
    }

    /// A `session.subscribe` stamped by the hub — ownership and role are both read off `caller`.
    fn viewer_subscribe(stream: &str, after_seq: u64) -> Frame {
        let mut f = Frame::request(
            method::SESSION_SUBSCRIBE,
            SessionSubscribe {
                session_id: stream.into(),
                after_seq,
            },
        );
        f.caller = Some(crate::protocol::CallerClaim {
            account_id: Some("acct-1".into()),
            role: "viewer".into(),
            workspace_id: "ws-a".into(),
        });
        f
    }

    /// Pins that `session.subscribe` still backfills after a disconnect once the tab has been
    /// closed and reopened.
    ///
    /// `session.unwatch` goes `take_watch` → `Journal::forget`, and forget permanently turns off
    /// "does this stream keep frames". A reopen path that does not turn it back on itself takes
    /// no frame into the ring from the second follow on: a reconnecting viewer gets an empty
    /// replay plus a `from_seq` that declares the history from the disconnected window
    /// permanently lost.
    #[tokio::test]
    async fn a_rewatched_stream_reopens_its_ring_so_a_reconnecting_viewer_still_backfills() {
        let mut d = watching_daemon();
        let (frames, _frames_rx) = mpsc::channel(8);
        let id = watch_stream_id("ws-a", "thread-1");

        // The page opens for the first time: install the tail, and it reports one frame.
        install_watch(
            &mut d.journal,
            &mut d.watches,
            id.clone(),
            watching_tail(&id),
        );
        d.journal.record(&id, watched_item());

        // Close the tab. This one line is what `session.unwatch` does (idle reaping and
        // `WatchEnded` likewise), and the `Journal::forget` inside it closes this stream's ring
        // permanently.
        d.take_watch(&id)
            .expect("the tail just installed is still in the table");

        // They open the page again: a second tail on the same watch stream id, also reporting one
        // frame.
        install_watch(
            &mut d.journal,
            &mut d.watches,
            id.clone(),
            watching_tail(&id),
        );
        assert_eq!(
            d.journal.record(&id, watched_item()).seq,
            Some(2),
            "seq must keep going; restarting from 1 lets the hub swallow the new opening as a (stream, seq) duplicate of an old frame"
        );

        // Their connection hiccups, so they follow up with `session.subscribe(after_seq = 1)`.
        let reply = d
            .dispatch(&viewer_subscribe(&id, 1), &frames)
            .await
            .expect("a watch stream is subscribable like a session");
        let reply: SessionSubscribeResult =
            serde_json::from_value(reply).expect("subscribe answers with a SessionSubscribeResult");

        assert_eq!(
            d.deferred.iter().filter_map(|f| f.seq).collect::<Vec<_>>(),
            vec![2],
            "a reopened watch stream keeps entering the journal's ring; otherwise no transcript line from the disconnected window can be backfilled"
        );
        assert_eq!(
            reply.from_seq, 2,
            "`from_seq` names the oldest frame still held; jumping to the next unsent seq tells the viewer all history below it is gone"
        );
    }
}
