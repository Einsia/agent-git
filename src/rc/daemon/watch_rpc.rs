use super::dispatch::install_watch;
use super::*;
use std::path::Path;

/// How long a newly installed tail yields to the `session.watch` response before it starts
/// sending replay frames.
///
/// The hub registers "which workspace this stream belongs to" only on receiving that response;
/// frames that arrive before the registration are fanned out to an empty channel. See
/// [`install_watch`].
pub(super) const WATCH_RESPONSE_HEADSTART: std::time::Duration =
    std::time::Duration::from_millis(500);

const MAX_WATCH_REQUESTS: usize = 32;
const MAX_WATCH_SCANS: usize = 4;

pub(super) struct WatchScan {
    request: SessionWatch,
    snapshot: sessions::LocalSessionSnapshot,
}

pub(super) struct PreparedWatch {
    request: SessionWatch,
    roots: policy::CanonicalRoots,
    runtime: String,
    cwd: PathBuf,
    protection_repo: Option<PathBuf>,
    source: WatchSource,
    seed: Option<Frame>,
    from_line: u64,
    total_lines: u64,
    absolute_lines: bool,
    before_cursor: u64,
    history_error: Option<String>,
    native_inbox: Option<String>,
    settings: Option<crate::rc::native_settings::Settings>,
    settings_boundary: Option<u64>,
}

enum WatchSource {
    File {
        path: PathBuf,
        offset: u64,
        handle: Option<same_file::Handle>,
    },
    Native {
        source: crate::adapter::native_snapshot::Source,
        snapshot: Vec<u8>,
    },
}

impl WatchScan {
    pub(super) fn run(self) -> Result<PreparedWatch, RpcError> {
        let Self { request, snapshot } = self;
        let roots = snapshot.roots.clone();
        let local = snapshot
            .scan(LocalSessionScan::Locate)
            .into_iter()
            .find(|local| local.runtime_session_id == request.session_id)
            .ok_or_else(|| {
                RpcError::new(
                    ErrorCode::SessionNotFound,
                    "no local session under this workspace's folders",
                )
                .with_hint("refresh the session list; its folder may no longer be bound")
            })?;
        Self::prepare_source(request, roots, local)
    }

    fn prepare_source(
        request: SessionWatch,
        roots: policy::CanonicalRoots,
        local: LocalSession,
    ) -> Result<PreparedWatch, RpcError> {
        let runtime = local.runtime;
        let cwd = policy::require_within(Path::new(&local.cwd), &roots)
            .map_err(|error| RpcError::new(ErrorCode::PathNotAllowed, error.to_string()))?;
        let protection_repo =
            crate::rc::protection::native_repository(&runtime, &request.session_id, &cwd).map_err(
                |_| {
                    RpcError::new(
                        ErrorCode::RuntimeUnavailable,
                        "session protection context is unavailable",
                    )
                },
            )?;
        let (source, from_line, total_lines, absolute_lines) = if runtime == "opencode" {
            use crate::adapter::{Adapter, native_snapshot::Limits, opencode::OpenCode};
            let source = OpenCode
                .lookup_native_readonly(&request.session_id, Limits::default())
                .map_err(|error| RpcError::new(ErrorCode::SessionNotFound, error.to_string()))?;
            let snapshot =
                crate::rc::supervisor::native_records::read_watch_snapshot_blocking(&source, &cwd)
                    .map_err(|error| {
                        RpcError::new(ErrorCode::SessionNotFound, error.to_string())
                    })?;
            let total = std::str::from_utf8(&snapshot)
                .map_err(|error| RpcError::new(ErrorCode::Internal, error.to_string()))?
                .lines()
                .count() as u64;
            (
                WatchSource::Native { source, snapshot },
                total.saturating_sub(WATCH_BACKFILL_LINES),
                total,
                true,
            )
        } else {
            let adapter = crate::adapter::get(&runtime)
                .map_err(|error| RpcError::new(ErrorCode::RuntimeUnavailable, error.to_string()))?;
            let path = adapter
                .resolve(&request.session_id, Some(&cwd))
                .ok_or_else(|| {
                    RpcError::new(
                        ErrorCode::SessionNotFound,
                        "cannot locate this session's transcript",
                    )
                })?;
            let (offset, from, total, absolute, handle) = tail_window(&path, WATCH_BACKFILL_LINES);
            if handle.is_none() {
                return Err(RpcError::new(
                    ErrorCode::RuntimeUnavailable,
                    "Native history cannot be read",
                ));
            }
            (
                WatchSource::File {
                    path,
                    offset,
                    handle,
                },
                from,
                total,
                absolute,
            )
        };
        let (before_cursor, history_error) = match &source {
            WatchSource::File { path, offset, .. } => history_cursor(&runtime, path, *offset),
            WatchSource::Native { .. } => (0, None),
        };
        let seed = match &source {
            WatchSource::File { path, offset, .. } => watch_seed_event(&runtime, path, *offset),
            WatchSource::Native { .. } => None,
        };
        let settings_boundary = match &source {
            WatchSource::File {
                handle: Some(handle),
                ..
            } if runtime == "codex" => handle
                .as_file()
                .metadata()
                .ok()
                .map(|metadata| metadata.len()),
            _ => None,
        };
        let settings = match &source {
            WatchSource::File { path, .. } if runtime == "codex" => Some(
                crate::rc::native_settings::read_codex(path, &request.session_id),
            ),
            _ => None,
        };
        Ok(PreparedWatch {
            request,
            roots,
            runtime,
            cwd,
            protection_repo,
            source,
            seed,
            from_line,
            total_lines,
            absolute_lines,
            before_cursor,
            history_error,
            native_inbox: None,
            settings,
            settings_boundary,
        })
    }
}

fn history_cursor(runtime: &str, path: &Path, offset: u64) -> (u64, Option<String>) {
    if runtime == "codex" {
        return match crate::rc::local_history::watch_cursor(path, offset) {
            Ok(cursor) => (cursor, None),
            Err(error) => (
                0,
                Some(format!("Native history paging is unavailable: {error}")),
            ),
        };
    }
    let _ = (runtime, path);
    (offset, None)
}

impl Daemon {
    pub(super) fn prepare_watch_scan(&self, frame: &Frame) -> Result<WatchScan, RpcError> {
        let caller = caller_scope(frame)?;
        require_role(&caller, method::SESSION_WATCH)?;
        let request: SessionWatch = frame.params_as()?;
        if !self.mirror.has_workspace(&caller.workspace_id) {
            return Err(RpcError::new(
                ErrorCode::WorkspaceNotFound,
                "workspace is not bound on this machine",
            ));
        }
        Ok(WatchScan {
            request,
            snapshot: self.local_session_scan(&caller.workspace_id),
        })
    }

    pub(super) fn finish_watch_scan(
        &mut self,
        frame: &Frame,
        prepared: PreparedWatch,
        frames: &mpsc::Sender<Frame>,
    ) -> Result<serde_json::Value, RpcError> {
        let caller = caller_scope(frame)?;
        require_role(&caller, method::SESSION_WATCH)?;
        let p: SessionWatch = frame.params_as()?;
        let PreparedWatch {
            request,
            roots,
            runtime,
            cwd,
            protection_repo,
            source,
            seed,
            from_line,
            total_lines,
            absolute_lines,
            before_cursor,
            history_error,
            native_inbox,
            settings,
            mut settings_boundary,
        } = prepared;
        if request != p {
            return Err(RpcError::new(
                ErrorCode::MalformedFrame,
                "watch preparation belongs to another request",
            ));
        }
        if !self.mirror.has_workspace(&caller.workspace_id)
            || self.mirror.roots(&caller.workspace_id) != roots
        {
            return Err(RpcError::new(
                ErrorCode::WorkspaceNotFound,
                "workspace folders changed while opening this session; refresh the list",
            ));
        }
        let current_cwd = policy::require_within(&cwd, &roots)
            .map_err(|error| RpcError::new(ErrorCode::PathNotAllowed, error.to_string()))?;
        if current_cwd != cwd {
            return Err(RpcError::new(
                ErrorCode::PathNotAllowed,
                "session folder changed while opening it",
            ));
        }
        if self
            .sessions
            .values()
            .any(|session| session.runtime_thread_id.as_deref() == Some(p.session_id.as_str()))
        {
            return Err(RpcError::new(
                ErrorCode::SessionBusy,
                "session is now supervised by this machine; refresh the list",
            ));
        }
        let project_id = self
            .mirror
            .workspaces
            .get(&caller.workspace_id)
            .and_then(|projects| {
                projects
                    .iter()
                    .find(|(_, root)| cwd.starts_with(Path::new(root)))
                    .map(|(id, _)| id.clone())
            });
        // The stream id comes from the thread id — several people watching the same
        // session still share one stream and one run of seqs.
        let watch_id = watch_stream_id(&caller.workspace_id, &p.session_id);

        let now = chrono::Utc::now().to_rfc3339();
        let info = SessionInfo {
            session_id: watch_id.clone(),
            runtime_session_id: Some(p.session_id.clone()),
            workspace_id: p.workspace_id.clone(),
            project_id,
            runtime: runtime.clone(),
            agent: None,
            branch: None,
            status: SessionStatus::Idle,
            last_seq: 0,
            gist: None,
            title: None,
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
            permission_mode: settings
                .as_ref()
                .and_then(|settings| settings.permission_mode),
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
        // A read-only follow and a supervised session take the same outbound
        // path, so they share the daemon's secret filter: loading a copy here
        // freezes a snapshot on this stream, which keeps allowing by the old
        // rules after `agit rc secrets reload`.
        let secret_filter = self.secret_filter.clone();
        let mut redactor = crate::domain::redact::Redactor::with_registered(
            crate::domain::redact::Persona::this_machine(),
            secret_filter,
        )
        .for_device_control();
        if let Some(root) = &protection_repo {
            redactor = redactor.with_repository(root).map_err(|_| {
                RpcError::new(
                    ErrorCode::RuntimeUnavailable,
                    "session protection context is unavailable",
                )
            })?;
            redactor = redactor.with_native_context(&runtime, &request.session_id, &cwd, root);
        }
        let model_settings = settings
            .as_ref()
            .map(|settings| redactor.scrub_json(&settings.model()).value);
        match self.watches.get_mut(&watch_id) {
            // Someone is already watching (and that tail really is alive): add a
            // subscriber rather than start a second one.
            Some(w) => {
                w.info.permission_mode = info.permission_mode;
                if let Some(owner) = frame.authority.watch_owner() {
                    w.shared_viewers.insert(owner, frame.authority.clone());
                } else {
                    *w.viewers.entry(caller_key(&caller)).or_insert(0) += 1;
                }
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
                let active_at = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(now_secs()));
                let active = active_at.clone();
                let frames = frames.clone();
                let notes = self.notes.clone();
                let stream = watch_id.clone();
                let rt = runtime.clone();
                let handle = tokio::spawn(async move {
                    // Read from the start of the window instead of reading from the
                    // beginning and discarding — the latter costs memory the size of
                    // the whole transcript.
                    let (mut tailer, native_source, mut initial_snapshot) = match source {
                        WatchSource::File {
                            path,
                            offset,
                            handle,
                        } => (
                            Some(crate::rc::tail::Tailer::at(
                                path,
                                offset,
                                from_line,
                                handle,
                                WATCH_BACKFILL_LINES,
                            )),
                            None,
                            None,
                        ),
                        WatchSource::Native { source, snapshot } => {
                            (None, Some(source), Some(snapshot))
                        }
                    };
                    // Let the `session.watch` **response** out first: the hub registers
                    // "which workspace this stream belongs to" only on that response,
                    // and replay frames that arrive before the registration fan out to
                    // an empty channel. Arriving early loses nothing — the frames are in
                    // the journal's ring, and a viewer replays them with a
                    // `session.subscribe`.
                    tokio::time::sleep(WATCH_RESPONSE_HEADSTART).await;
                    if let Some(mut frame) = seed {
                        frame.stream = Some(stream.clone());
                        if frames.send(frame).await.is_err() {
                            return;
                        }
                    }
                    let mut native_records =
                        crate::rc::supervisor::native_records::NativeRecords::default();
                    let mut initial = true;
                    loop {
                        if let Some(source) = &native_source {
                            let bytes = if let Some(bytes) = initial_snapshot.take() {
                                bytes
                            } else {
                                let Ok(bytes) =
                                    crate::rc::supervisor::native_records::read_watch_snapshot(
                                        source.clone(),
                                        cwd.clone(),
                                    )
                                    .await
                                else {
                                    history_status(
                                        &frames,
                                        &stream,
                                        "failed",
                                        Some("source_unreadable"),
                                    )
                                    .await;
                                    break;
                                };
                                bytes
                            };
                            let Ok((items, _)) =
                                native_records.project_window(&bytes, false, &redactor, from_line)
                            else {
                                history_status(&frames, &stream, "failed", Some("invalid_record"))
                                    .await;
                                break;
                            };
                            if !items.is_empty() {
                                active.store(
                                    crate::rc::daemon::now_secs(),
                                    std::sync::atomic::Ordering::Release,
                                );
                            }
                            for item in items {
                                let mut frame = Frame::notification(method::ITEM_COMPLETED, item);
                                frame.stream = Some(stream.clone());
                                if frames.send(frame).await.is_err() {
                                    return;
                                }
                            }
                            if initial {
                                history_status(&frames, &stream, "complete", None).await;
                                initial = false;
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            continue;
                        }
                        // Reap once the transcript is gone (the session was cleaned up,
                        // the directory was deleted).
                        let Some(tailer) = tailer.as_mut() else {
                            break;
                        };
                        if !tailer.path().exists() {
                            history_status(&frames, &stream, "failed", Some("source_missing"))
                                .await;
                            break;
                        }
                        let batch = if rt == "codex" {
                            tailer.poll_codex().map(|batch| (batch.mode, batch.lines))
                        } else {
                            tailer
                                .poll()
                                .map(|lines| (crate::rc::codex_history::HistoryMode::Model, lines))
                        };
                        let Ok((mode, lines)) = batch else {
                            history_status(&frames, &stream, "failed", Some("source_unreadable"))
                                .await;
                            break;
                        };
                        if tailer.take_reset() {
                            history_status(&frames, &stream, "reset", None).await;
                            initial = true;
                            settings_boundary = None;
                        }
                        if lines.iter().any(|line| {
                            !line.text.trim().is_empty()
                                && serde_json::from_str::<serde_json::Value>(&line.text).is_err()
                        }) {
                            history_status(&frames, &stream, "failed", Some("invalid_record"))
                                .await;
                            break;
                        }
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
                            for mut frame in
                                watch_frames(&rt, &redactor, &lines, mode, settings_boundary)
                            {
                                frame.stream = Some(stream.clone());
                                if matches!(
                                    frame.method(),
                                    "session.model" | "session.permissionMode"
                                ) {
                                    frame.params.as_mut().unwrap()["session_id"] =
                                        stream.clone().into();
                                }
                                if frames.send(frame).await.is_err() {
                                    return;
                                }
                            }
                        }
                        if initial && !tailer.has_pending() {
                            history_status(&frames, &stream, "complete", None).await;
                            initial = false;
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
                        viewers: if frame.authority.watch_owner().is_none() {
                            [(caller_key(&caller), 1usize)].into_iter().collect()
                        } else {
                            Default::default()
                        },
                        shared_viewers: frame
                            .authority
                            .watch_owner()
                            .map(|owner| (owner, frame.authority.clone()))
                            .into_iter()
                            .collect(),
                        generation,
                    },
                );
            }
        }

        Ok(serde_json::to_value(SessionWatchResult {
            before_cursor,
            history_error,
            session: self.stamped(
                self.watches
                    .get(&watch_id)
                    .map(|watch| watch.info.clone())
                    .unwrap_or(info),
            ),
            from_line,
            total_lines,
            absolute_lines,
            read_only: true,
            native_inbox,
            model_settings,
        })
        .unwrap())
    }
}

/// Requests on a shared stream stay ordered even if an intermediate worker is cancelled.
pub(super) struct WatchRpcQueue {
    tails: HashMap<String, tokio::sync::watch::Receiver<bool>>,
    requests: Arc<tokio::sync::Semaphore>,
    scans: Arc<tokio::sync::Semaphore>,
}

impl Default for WatchRpcQueue {
    fn default() -> Self {
        Self {
            tails: HashMap::new(),
            requests: Arc::new(tokio::sync::Semaphore::new(MAX_WATCH_REQUESTS)),
            scans: Arc::new(tokio::sync::Semaphore::new(MAX_WATCH_SCANS)),
        }
    }
}

pub(super) struct WatchRpcTicket {
    previous: Option<tokio::sync::watch::Receiver<bool>>,
    finished: tokio::sync::oneshot::Sender<()>,
    scans: Arc<tokio::sync::Semaphore>,
}

async fn barrier(previous: &mut Option<tokio::sync::watch::Receiver<bool>>) {
    if let Some(previous) = previous {
        let _ = previous.wait_for(|done| *done).await;
    }
}

impl WatchRpcQueue {
    pub(super) fn reserve(&mut self, frame: &Frame) -> Result<WatchRpcTicket, RpcError> {
        let caller = caller_scope(frame)?;
        require_role(&caller, frame.method())?;
        if !matches!(
            frame.method(),
            method::SESSION_WATCH | method::SESSION_UNWATCH
        ) {
            return Err(RpcError::new(
                ErrorCode::MalformedFrame,
                "request is not a watch operation",
            ));
        }
        let request: SessionWatch = frame.params_as()?;
        let permit = self.requests.clone().try_acquire_owned().map_err(|_| {
            RpcError::new(
                ErrorCode::SessionBusy,
                "session opening is busy; retry shortly",
            )
        })?;
        self.tails.retain(|_, done| !*done.borrow());
        let (done, receiver) = tokio::sync::watch::channel(false);
        let previous = self.tails.insert(
            watch_stream_id(&caller.workspace_id, &request.session_id),
            receiver,
        );
        let (finished, completion) = tokio::sync::oneshot::channel();
        let mut predecessor = previous.clone();
        // Cancellation releases a request only after its predecessor has released the stream.
        tokio::spawn(async move {
            let _permit = permit;
            barrier(&mut predecessor).await;
            let _ = completion.await;
            done.send_replace(true);
        });
        Ok(WatchRpcTicket {
            previous,
            finished,
            scans: self.scans.clone(),
        })
    }
}

async fn stopping(stop: &mut tokio::sync::watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopped| *stopped).await;
}

impl WatchRpcTicket {
    pub(super) async fn serve(
        self,
        daemon: Arc<Mutex<Daemon>>,
        outbound: crate::rc::outbound::OutboundTx,
        frames: mpsc::Sender<Frame>,
        frame: Frame,
        epoch: u64,
        stop: tokio::sync::watch::Receiver<bool>,
    ) {
        self.serve_with(
            daemon,
            outbound,
            frames,
            (frame, epoch),
            stop,
            WatchScan::run,
        )
        .await;
    }

    async fn serve_with(
        self,
        daemon: Arc<Mutex<Daemon>>,
        outbound: crate::rc::outbound::OutboundTx,
        frames: mpsc::Sender<Frame>,
        request: (Frame, u64),
        mut stop: tokio::sync::watch::Receiver<bool>,
        scan: impl FnOnce(WatchScan) -> Result<PreparedWatch, RpcError> + Send + 'static,
    ) {
        let (frame, epoch) = request;
        let Self {
            mut previous,
            finished: _finished,
            scans,
        } = self;
        let Some(id) = frame.id.clone() else { return };
        tokio::select! {
            _ = barrier(&mut previous) => {},
            _ = stopping(&mut stop) => return,
        }
        let prepared = {
            let mut state = daemon.lock().await;
            if *stop.borrow() || !connection_epoch_is_current(&state.settlement, epoch) {
                return;
            }
            if frame.method() == method::SESSION_UNWATCH {
                let result = state.dispatch(&frame, &frames).await;
                let response = match result {
                    Ok(value) => Frame::response(id, value),
                    Err(error) => Frame::error_response(id, error),
                };
                let _ = outbound.send(response);
                return;
            }
            state.prepare_watch_scan(&frame)
        };
        let result = match prepared {
            Err(error) => Err(error),
            Ok(prepared) => {
                let permit = tokio::select! {
                    permit = scans.acquire_owned() => match permit { Ok(permit) => permit, Err(_) => return },
                    _ = stopping(&mut stop) => return,
                };
                {
                    let state = daemon.lock().await;
                    if *stop.borrow() || !connection_epoch_is_current(&state.settlement, epoch) {
                        return;
                    }
                }
                let scanned = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    scan(prepared)
                });
                let scanned = tokio::select! {
                    result = scanned => result.map_err(|_| RpcError::new(ErrorCode::Internal,
                        "session history preparation worker failed")).and_then(|result| result),
                    _ = stopping(&mut stop) => return,
                };
                let mut scanned = scanned;
                if let Ok(prepared) = &mut scanned
                    && prepared.runtime == "codex"
                    && let Some(codex) = crate::adapter::which("codex")
                    && crate::rc::native_inbox::queue_available(codex).await
                {
                    prepared.native_inbox = Some("codex_queue".into());
                }
                let mut state = daemon.lock().await;
                if *stop.borrow() || !connection_epoch_is_current(&state.settlement, epoch) {
                    return;
                }
                scanned.and_then(|prepared| state.finish_watch_scan(&frame, prepared, &frames))
            }
        };
        let state = daemon.lock().await;
        if *stop.borrow() || !connection_epoch_is_current(&state.settlement, epoch) {
            return;
        }
        let response = match result {
            Ok(value) => Frame::response(id, value),
            Err(error) => Frame::error_response(id, error),
        };
        let _ = outbound.send(response);
    }
}

#[cfg(test)]
#[path = "tests/watch_rpc.rs"]
mod tests;

/// Lifecycle markers stay interleaved with their protected source records.
fn watch_frames(
    runtime: &str,
    redactor: &crate::domain::redact::Redactor,
    lines: &[crate::rc::tail::TailedLine],
    mode: crate::rc::codex_history::HistoryMode,
    settings_boundary: Option<u64>,
) -> Vec<Frame> {
    let (items, _) =
        crate::rc::supervisor::items_from_lines_with_mode(runtime, redactor, lines, mode);
    let mut items = items.into_iter().peekable();
    let mut frames = Vec::new();
    for line in lines {
        if let Some(frame) = watch_turn_event(runtime, &line.text) {
            frames.push(frame);
        }
        // Replayed contexts precede the current native settings snapshot.
        let current_observation = settings_boundary.is_none_or(|boundary| {
            line.source
                .as_deref()
                .and_then(|source| source.rsplit(':').next()?.parse::<u64>().ok())
                .is_some_and(|start| start.saturating_add(line.text.len() as u64 + 1) > boundary)
        });
        if runtime == "codex"
            && current_observation
            && let Some(settings) = crate::rc::native_settings::context(&line.text)
        {
            frames.push(Frame::notification(
                "session.model",
                serde_json::json!({
                    "settings":redactor.scrub_json(&settings.model()).value
                }),
            ));
            if let Some(mode) = settings.permission_mode {
                frames.push(Frame::notification(
                    "session.permissionMode",
                    serde_json::json!({
                        "mode":mode,"applied":"immediate"
                    }),
                ));
            }
        }
        while items.peek().is_some_and(|item| item.line == line.lineno) {
            frames.push(Frame::notification(
                method::ITEM_COMPLETED,
                items.next().unwrap(),
            ));
        }
    }
    frames
}

/// Transcript lifecycle markers describe external turns without claiming their control channel.
fn watch_turn_event(runtime: &str, line: &str) -> Option<Frame> {
    let raw: serde_json::Value = serde_json::from_str(line).ok()?;
    let payload = raw.get("payload")?;
    if runtime != "codex" || raw["type"] != "event_msg" {
        return None;
    }
    let id = payload
        .get("turn_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match payload.get("type")?.as_str()? {
        "task_started" => Some(Frame::notification(
            "turn.started",
            serde_json::json!({"turn_id":id}),
        )),
        "task_complete" | "turn_aborted" => Some(Frame::notification(
            "turn.completed",
            serde_json::json!({"turn_id":id,"outcome":if payload["type"] == "turn_aborted" { "interrupted" } else { "ok" }}),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod watch_activity_tests {
    use super::*;

    #[test]
    fn batched_watch_keeps_content_inside_its_turn() {
        let records = [
            (
                3,
                serde_json::json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-a"}}),
            ),
            (7, serde_json::json!({"type":"session_meta","payload":{}})),
            (
                11,
                serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"First answer"}]}}),
            ),
            (
                19,
                serde_json::json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"turn-a"}}),
            ),
            (
                23,
                serde_json::json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn-b"}}),
            ),
            (
                29,
                serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Second answer"}]}}),
            ),
            (
                31,
                serde_json::json!({"type":"event_msg","payload":{"type":"turn_aborted","turn_id":"turn-b"}}),
            ),
        ];
        let lines: Vec<_> = records
            .into_iter()
            .map(|(lineno, record)| crate::rc::tail::TailedLine {
                source: None,
                lineno,
                text: record.to_string(),
            })
            .collect();
        let redactor = crate::domain::redact::Redactor::new(Default::default());
        let frames = watch_frames(
            "codex",
            &redactor,
            &lines,
            crate::rc::codex_history::HistoryMode::Model,
            None,
        );
        assert_eq!(
            frames
                .iter()
                .map(|frame| frame.method.as_deref().unwrap())
                .collect::<Vec<_>>(),
            [
                "turn.started",
                "item.completed",
                "turn.completed",
                "item.completed",
                "turn.started",
                "item.completed",
                "turn.completed"
            ]
        );
        assert_eq!(
            frames[1].params.as_ref().unwrap()["event"]["text"],
            "First answer"
        );
        assert_eq!(frames[1].params.as_ref().unwrap()["line"], 11);
        assert_eq!(frames[3].params.as_ref().unwrap()["line"], 19);
        assert_eq!(
            frames[3].params.as_ref().unwrap()["event"]["kind"],
            "turn_end"
        );
        assert_eq!(
            frames[5].params.as_ref().unwrap()["event"]["text"],
            "Second answer"
        );
        assert_eq!(frames[5].params.as_ref().unwrap()["line"], 29);
        assert_eq!(frames[6].params.as_ref().unwrap()["outcome"], "interrupted");
    }

    #[test]
    fn native_turn_markers_do_not_treat_content_or_quiet_as_completion() {
        for (kind, method) in [
            ("task_started", "turn.started"),
            ("task_complete", "turn.completed"),
            ("turn_aborted", "turn.completed"),
        ] {
            let line =
                serde_json::json!({"type":"event_msg", "payload":{"type":kind,"turn_id":"turn-a"}})
                    .to_string();
            let frame = watch_turn_event("codex", &line).unwrap();
            assert_eq!(frame.method.as_deref(), Some(method));
        }
        assert!(watch_turn_event("codex", r#"{"type":"event_msg","payload":{"type":"agent_message","message":"task_complete"}}"#).is_none());
        assert!(watch_turn_event("codex", "").is_none());
    }
}

/// Recover lifecycle state before the display window without retaining transcript contents.
fn watch_seed_event(runtime: &str, path: &std::path::Path, before: u64) -> Option<Frame> {
    use std::io::{BufRead, Read};
    if runtime != "codex" || before == 0 {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut reader = std::io::BufReader::new(file.take(before));
    let mut last = None;
    let mut skipping = false;
    loop {
        let mut bytes = Vec::new();
        let count = reader
            .by_ref()
            .take(65537)
            .read_until(b'\n', &mut bytes)
            .ok()?;
        if count == 0 {
            break;
        }
        let complete = bytes.last() == Some(&b'\n');
        if !skipping
            && complete
            && let Ok(line) = std::str::from_utf8(&bytes)
            && let Some(frame) = watch_turn_event(runtime, line)
        {
            last = Some(frame);
        }
        skipping = !complete;
    }
    last
}

#[cfg(test)]
mod watch_seed_tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn opening_inside_a_long_turn_recovers_the_marker_outside_the_display_window() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_started\",\"turn_id\":\"long-turn\"}}}}").unwrap();
        writeln!(file, "{}", "x".repeat(200_000)).unwrap();
        for _ in 0..WATCH_BACKFILL_LINES + 1 {
            writeln!(file, "{{\"type\":\"token_usage_record\"}}").unwrap();
        }
        let before = file.as_file().metadata().unwrap().len();
        writeln!(file, "{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"task_complete\",\"turn_id\":\"long-turn\"}}}}").unwrap();
        let event = watch_seed_event("codex", file.path(), before).unwrap();
        assert_eq!(event.method(), "turn.started");
        let event = watch_seed_event(
            "codex",
            file.path(),
            file.as_file().metadata().unwrap().len(),
        )
        .unwrap();
        assert_eq!(event.method(), "turn.completed");
    }
}

async fn history_status(
    frames: &mpsc::Sender<Frame>,
    stream: &str,
    status: &str,
    error: Option<&str>,
) {
    let mut frame = Frame::notification(
        "session.history.status",
        serde_json::json!({"status":status,"error":error}),
    );
    frame.stream = Some(stream.to_string());
    let _ = frames.send(frame).await;
}
