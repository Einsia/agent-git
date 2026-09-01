use super::*;

impl Daemon {
    /// Local sessions on this machine that can be taken over.
    ///
    /// The test is **the session record's cwd lands inside a project this workspace binds** —
    /// not which directory the file sits in (codex splits directories by date; the path carries
    /// no project information).
    ///
    /// Enumerating opens no transcript; only the most recent [`LOCAL_GIST_BUDGET`] sessions in
    /// the web listing are parsed once each for a gist, so the cost is a constant rather than
    /// "times the number of sessions on disk". The internal locate for resume / watch passes
    /// [`LocalSessionScan::Locate`] and opens no transcript for a gist at all.
    pub(super) fn local_sessions(
        &self,
        workspace_id: &str,
        purpose: LocalSessionScan,
    ) -> Vec<LocalSession> {
        let roots = self.mirror.roots(workspace_id);
        if roots.is_empty() {
            return vec![];
        }
        let store = crate::domain::store::Store::open().ok().flatten();
        let mut out: Vec<LocalSession> = vec![];

        for adapter in crate::adapter::all() {
            if !adapter.installable() || !adapter.available() {
                continue;
            }
            for root in &roots {
                // Every adapter filters by **this project directory** rather than scanning the
                // whole store: codex uses the `threads` table's
                // `(archived, cwd, updated_at_ms DESC)` index, Claude Code is one readdir of
                // `projects/<cwd-slug>/`. So the cost follows "how many sessions this project
                // has", not how many rollouts piled up on disk.
                let Ok(mut refs) = adapter.sessions_for(root) else {
                    continue;
                };
                // The index is already in reverse time order, but the file fallback path is
                // not — sort once and then truncate, so "the most recently talked to" always
                // sits within the first PER_PROJECT_LIMIT.
                refs.sort_by_key(|r| std::cmp::Reverse(r.mtime));
                refs.truncate(PER_PROJECT_LIMIT);
                for r in refs {
                    // A session already under supervision is no longer listed as "takeable".
                    if self
                        .sessions
                        .values()
                        .any(|l| l.runtime_thread_id.as_deref() == Some(r.id.as_str()))
                    {
                        continue;
                    }
                    let link = store
                        .as_ref()
                        .and_then(|s| crate::domain::link::get(s, r.runtime, &r.id));
                    out.push(LocalSession {
                        runtime_session_id: r.id.clone(),
                        runtime: r.runtime.to_string(),
                        cwd: r
                            .cwd
                            .clone()
                            .unwrap_or_else(|| root.to_string_lossy().to_string()),
                        modified_at: rfc3339(r.mtime),
                        // The codex index gives an opening prompt for free; Claude Code has
                        // no index, so leave None and fill it in below within the budget.
                        gist: r.gist.clone(),
                        adopted: link.is_some(),
                        agent: link.and_then(|l| l.agent),
                        likely_active: recently_written(r.mtime),
                    });
                }
            }
        }

        finish_local_sessions(out, purpose, |item| {
            let adapter = crate::adapter::get(&item.runtime).ok()?;
            let path = adapter.resolve(
                &item.runtime_session_id,
                Some(std::path::Path::new(&item.cwd)),
            )?;
            adapter.parse_at(&path).ok()?.gist(80)
        })
    }

    /// Take over a local session.
    ///
    /// Same semantics as `agit resume`: the fast path resumes natively (the harness's own
    /// `--resume`), content untouched, id unchanged. The slow path (across harnesses,
    /// materializing a new id) is not done in RC — that changes something on the user's machine
    /// without being asked.
    pub(super) async fn resume_session(
        &mut self,
        p: SessionResume,
        caller: &crate::protocol::CallerClaim,
        frames: &mpsc::Sender<Frame>,
    ) -> Result<serde_json::Value, RpcError> {
        if !self.mirror.has_workspace(&p.workspace_id) {
            return Err(RpcError::new(
                ErrorCode::WorkspaceNotFound,
                format!("workspace {} is not bound on this machine", p.workspace_id),
            ));
        }
        // Already supervised: hand it straight back rather than starting a second process to
        // fight over the same transcript file.
        //
        // **Look it up by workspace** (`supervised_in`). A `find` over `self.sessions.values()`
        // that does not compare workspaces lets a member of B hand in one of A's harness thread
        // ids and get A's session `SessionInfo` back verbatim — and the hub registers that
        // response as "a session just created" into B's projection row, so B's operator passes
        // `session_belongs_to` from then on and A's stream starts fanning out to B's viewers.
        //
        // This branch **judges no danger**: it hands back only metadata `session.list` already
        // exposed, not one byte of transcript. Judging here would instead close off the
        // operator's last route to "what state is that dangerous session in now", while the
        // genuinely dangerous actions (sending a message, allowing an approval) each sit behind
        // their own gate.
        if self.ending_in(&p.session_id, caller) {
            return Err(RpcError::new(
                ErrorCode::SessionBusy,
                "that session ended while an accepted instruction is still settling",
            )
            .with_hint(
                "wait for the outstanding instruction to resolve before resuming this conversation",
            ));
        }
        if let Some(info) = self.supervised_in(&p.session_id, caller) {
            return Ok(serde_json::to_value(SessionResumeResult {
                session: self.stamped(info),
            })
            .unwrap());
        }

        // The durable mapping for logical ids (the main path after a daemon restart): the web
        // stores the `agit-...` id while the harness only knows its own thread id. The roster
        // joins the two ids back together and the session keeps one logical identity — for the
        // web, "the daemon restarted" does not exist.
        if let Some(entry) = self.roster.get(&p.session_id).cloned() {
            // Tenant boundary: a session resumes only from the workspace it is registered
            // under. Comparing cwd alone is not enough — the same directory (or a subdirectory
            // of it) can be bound once by each of two workspaces, and then B's request takes A's
            // session over under the same logical id, with every later event and permission
            // charged to B. Overlapping paths are not tenant isolation.
            if entry.workspace_id != p.workspace_id {
                return Err(RpcError::new(
                    ErrorCode::SessionNotFound,
                    format!("no session {} in this workspace", p.session_id),
                )
                .with_hint("that session belongs to a different workspace"));
            }
            // Resuming a session that **ever** ran without approvals is owner-only too: its
            // context may still hold what was read at that time with nobody reviewing it.
            //
            // **The question is about that transcript, not this row.** What `resume_from` hands
            // over below is `entry.thread_id` — one harness transcript, and one transcript can
            // be pointed at by several rows: when the same directory is bound once by each of
            // two workspaces, the workspace test in `logical_for_thread` requires each side to
            // mint its own logical id (see the comment on `take_over_local_session`), and the
            // danger bit is armed per row. So ws-a's row poisoned and ws-b's row clean is the
            // **designed** state; asking only about your own row makes ws-b's clean sibling a
            // master key to this transcript.
            //
            // When the ledger cannot be read (`history_lost`) **every session is treated as
            // dangerous**, and that branch of the test is still there — see
            // `Roster::transcript_ever_dangerous`.
            let danger = danger::authorize(
                &self.roster,
                caller,
                &entry.runtime,
                &entry.thread_id,
                &p.workspace_id,
                &entry.cwd,
            )?;
            // The danger pre-write row from `spawn_session` may carry an empty thread id (the
            // harness crashed before reporting a native id). An empty string is not a resumable
            // address: feeding it to `--resume` starts a **brand new** conversation wearing the
            // old session's logical identity and permission mode. Refuse honestly; the
            // transcript, if it exists at all, shows up in the local sessions list and is taken
            // over there by its real thread id.
            if entry.thread_id.is_empty() {
                return Err(RpcError::new(
                    ErrorCode::SessionNotFound,
                    "this session crashed before its harness reported a native id, so there is no thread to resume",
                )
                .with_hint(
                    "if its transcript exists it appears in the local sessions list; take it over from there",
                ));
            }
            let roots = self.mirror.roots(&p.workspace_id);
            let cwd = policy::require_within(std::path::Path::new(&entry.cwd), &roots)
                .map_err(|e| {
                    RpcError::new(ErrorCode::PathNotAllowed, e.to_string()).with_hint(
                        "that session's working directory is outside this workspace's bound folders",
                    )
                })?;
            if transcript_recently_written(&entry.runtime, &entry.thread_id, &cwd) {
                return Err(busy_error());
            }
            // The cell in the roster may hold legacy lineage that does not pass today's test.
            //
            // This path must not fail hard: the session belongs to the user, the roster is our
            // own ledger, and refusing to resume a whole conversation over one questionable
            // lineage plainly costs more. It must not pretend nothing is wrong either — a
            // session with no lineage runs normally and simply **settles no commit ever again**,
            // and `agit rc start --detach` points stderr at /dev/null, which is exactly the
            // deployment shape where this most needs to be seen. So: degrade to "no lineage",
            // and carry that word through to the web.
            let lineage = resume_lineage(
                self.settlement_feature(),
                &entry,
                p.agent.as_deref(),
                p.expected_agent_id.as_deref(),
                p.branch.as_deref(),
            )?;
            if lineage.is_none() && entry.agit_session.is_some() {
                eprintln!(
                    "agitd: session {} has legacy or unusable repository lineage; \
                     it will run but will not settle — start a new session after upgrading the hub",
                    p.session_id
                );
            }
            let (agent, branch) = match &lineage {
                Some(l) => (Some(l.slug()), Some(l.branch().to_string())),
                None => (None, None),
            };
            let now = chrono::Utc::now().to_rfc3339();
            let info = SessionInfo {
                session_id: p.session_id.clone(),
                workspace_id: p.workspace_id.clone(),
                project_id: entry.project_id.clone(),
                runtime: entry.runtime.clone(),
                agent,
                branch,
                status: SessionStatus::Idle,
                last_seq: 0,
                gist: None,
                // The monotonic bit is stamped by `spawn_session` from the slip above; it is
                // not inferred back from the current permission mode and not copied again here.
                dangerous: false,
                permission_mode: entry.restart_permission_mode(),
                created_at: now.clone(),
                updated_at: now,
            };
            let spec = LaunchSpec {
                cwd,
                resume_from: Some(entry.thread_id.clone()),
                agit_session: lineage,
                model: None,
                dangerous: false,
                // Resume brings back the guard it ran under.
                permission_mode: entry.restart_permission_mode(),
            };
            let session = self
                .spawn_session(info, spec, danger, frames, p.prompt, p.by)
                .await?;
            return Ok(serde_json::to_value(SessionResumeResult { session }).unwrap());
        }

        // **One** scan yields both the liveness bit and the launch coordinates. Asking
        // `local_sessions` for liveness and then letting `locate_local` scan again from scratch
        // puts both passes under the daemon's global mutex and opens the same Claude transcripts
        // for a gist twice. The internal locate needs no gist at all.
        let local = self.locate_local(&p.workspace_id, &p.session_id)?;
        self.take_over_local_session(local, p, caller, frames).await
    }

    /// Take over a session on this machine that was **opened in a terminal** — the half
    /// `session.resume` takes when it finds no logical identity.
    ///
    /// This sits apart from `resume_session` for exactly one reason: **so a unit test can really
    /// walk the owner-only gate on this path.** `locate_local` succeeds only with an installed
    /// harness (`which claude`) plus a real transcript on disk, so the whole takeover path is
    /// unreachable from a test — and it is the scene of the "a transcript that ran without checks
    /// gets minted into a clean identity" hole. Once the test is swapped for "ask this row" — a
    /// logical id fresh out of `mint_session_id` is forever clean to the ledger — an operator
    /// picks up everything that unchecked run read into its context; with this path unreachable,
    /// that edit turns no test red. The looser phrasing cannot leave the `roster` module (see
    /// [`Roster::transcript_ever_dangerous`](crate::rc::roster::Roster::transcript_ever_dangerous)),
    /// and the test itself comes from [`danger`](super::danger).
    pub(super) async fn take_over_local_session(
        &mut self,
        local: LocatedLocal,
        p: SessionResume,
        caller: &crate::protocol::CallerClaim,
        frames: &mpsc::Sender<Frame>,
    ) -> Result<serde_json::Value, RpcError> {
        // A live session cannot be taken over: `--resume` opens a second writer on the same
        // transcript file, and once the two streams of appends interleave both histories are
        // destroyed. This is data corruption, not an experience problem, so it is blocked here
        // instead of only hinted at in the UI.
        if local.likely_active {
            return Err(busy_error());
        }

        // Find which project this session belongs to — the cwd must land inside the allowlist,
        // or a takeover amounts to bypassing that allowlist to start an agent in an arbitrary
        // directory.
        let LocatedLocal {
            runtime,
            cwd,
            project_id,
            likely_active: _,
        } = local;
        let roots = self.mirror.roots(&p.workspace_id);
        let cwd = policy::require_within(&cwd, &roots).map_err(|e| {
            RpcError::new(ErrorCode::PathNotAllowed, e.to_string()).with_hint(
                "that session's working directory is outside this workspace's bound folders",
            )
        })?;

        // Taking over a session opened in a terminal: if it was ever registered, reuse that
        // logical id — one conversation has one identity, or the web grows two rows pointing at
        // the same transcript.
        // **Check who it belongs to before reusing the old identity.**
        //
        // **The workspace is part of the `logical_for_thread` test**, and that cell exists for
        // exactly this. The same directory can be bound once by each of two workspaces (the
        // comment on `watch_stream_id` is written for the same thing): A has session `agit-X`
        // (thread `t1`, cwd `/srv/app`), and B binds `/srv/app` too. Looking up by
        // `(runtime, thread_id)` alone lets B's operator send `session.resume(t1)` and run that
        // session under B, and `SessionNote::Bound` then **rewrites to B** the workspace on the
        // `agit-X` row in the roster. A's members get "it belongs to another workspace" for
        // their own conversation from then on, while B's viewers receive its events and lineage.
        //
        // (The danger bit does not follow this test: that question is "did this transcript ever
        // run without checks", and the answer must not change because a different workspace is
        // asking. See `authorize_thread_takeover` below.)
        //
        // If it belongs elsewhere, treat it as having no old identity: mint a new one and let
        // the two sides go their separate ways.
        let logical = self
            .roster
            .logical_for_thread(&runtime, &p.session_id, &p.workspace_id)
            .unwrap_or_else(crate::domain::meta::mint_session_id);
        // The hub fills in lineage (only it knows which repo this project maps to). Without it,
        // a session taken over from a terminal runs to the end and still **settles no commit** —
        // `agit commit --from-hook` cannot resolve which branch to record on.
        let prior = self.roster.get(&logical).cloned();
        // A roster row that already names a repository is this conversation's
        // first local identity claim. It wins over current wire params. In
        // particular, a legacy slug-only row cannot silently adopt the ID now
        // occupying that name. A clean row with neither field may accept the
        // first complete, negotiated identity (the checkout pin still gates it).
        let agit_session = match &prior {
            Some(entry) => resume_lineage(
                self.settlement_feature(),
                entry,
                p.agent.as_deref(),
                p.expected_agent_id.as_deref(),
                p.branch.as_deref(),
            )?,
            None => lineage_from_params(
                self.settlement_feature(),
                p.agent.as_deref(),
                p.expected_agent_id.as_deref(),
                p.branch.as_deref(),
            )?,
        };

        // **Has this session ever been dangerous.**
        //
        // The `logical_for_thread` call above just proved the roster may already hold it — that
        // record carries the monotonic `ever_dangerous` and the permission mode it last really
        // ran under. Writing `dangerous: false` / `Default` unconditionally here means: a session
        // an owner started with bypass comes back wearing a "clean" identity as soon as it is
        // resumed once more by **harness thread id**, and `SessionNote::Bound` then writes that
        // "clean" into the roster, washing the monotonic bit out **on disk**. From then on any
        // operator can drive it.
        //
        // One conversation has one identity, and one history.
        // The test lives in one place: `Roster::transcript_ever_dangerous`, taken from `danger`.
        //
        // Spelling the question out again here (`history_lost || prior.ever_dangerous`) is not
        // the same thing as the narrowed definition — the two answers are opposite when the
        // ledger was lost but this row **is written down again after that loss**. One question
        // with two definitions has a wrong one sooner or later.
        //
        // So this shares one judgement with `session.watch` and with the resume-by-logical-id
        // half above: besides reading the monotonic bit by logical identity, it treats as
        // dangerous the unknown thread of "an ownerless session on the same
        // `(runtime, workspace)` or the same directory that started dangerous and crashed before
        // `Bound`" — the ledger does not know that session's thread id, and the unrecognized id
        // in front of it may be exactly that one; minting a clean identity and letting it
        // through hands the operator everything that unchecked run read.
        let danger = danger::authorize(
            &self.roster,
            caller,
            &runtime,
            &p.session_id,
            &p.workspace_id,
            &cwd.to_string_lossy(),
        )?;
        let inherited_mode = prior
            .as_ref()
            .and_then(roster::Entry::restart_permission_mode)
            .unwrap_or(crate::protocol::PermissionMode::Default);

        let now = chrono::Utc::now().to_rfc3339();
        let info = SessionInfo {
            session_id: logical,
            workspace_id: p.workspace_id.clone(),
            project_id,
            runtime: runtime.clone(),
            agent: agit_session.as_ref().map(|l| l.slug()),
            branch: agit_session.as_ref().map(|l| l.branch().to_string()),
            status: SessionStatus::Idle,
            last_seq: 0,
            gist: None,
            // The judged bit is stamped by `spawn_session` — copying it here is one more place
            // that has to be right.
            dangerous: false,
            permission_mode: Some(inherited_mode),
            created_at: now.clone(),
            updated_at: now,
        };
        let spec = LaunchSpec {
            cwd,
            // This line is "keep talking where it left off": hand the harness back its own id.
            resume_from: Some(p.session_id.clone()),
            agit_session,
            model: None,
            dangerous: false,
            // It runs in the permission mode it last ran in. `None` (= Default) here means a
            // session deliberately confined to `plan` silently takes write access back on one
            // takeover.
            permission_mode: Some(inherited_mode),
        };
        let session = self
            .spawn_session(info, spec, danger, frames, p.prompt, p.by)
            .await?;
        Ok(serde_json::to_value(SessionResumeResult { session }).unwrap())
    }

    /// Local session → (runtime, cwd, project_id).
    pub(super) fn locate_local(
        &self,
        workspace_id: &str,
        runtime_session_id: &str,
    ) -> Result<LocatedLocal, RpcError> {
        locate_local_with(&self.mirror, workspace_id, runtime_session_id, || {
            self.local_sessions(workspace_id, LocalSessionScan::Locate)
        })
    }

    pub(super) async fn start_session(
        &mut self,
        p: SessionStart,
        caller: &crate::protocol::CallerClaim,
        frames: &mpsc::Sender<Frame>,
    ) -> Result<serde_json::Value, RpcError> {
        let start_id =
            negotiated_start_id(self.start_idempotency_feature(), p.start_id.as_deref())?;
        let mode = p
            .permission_mode
            .unwrap_or(crate::protocol::PermissionMode::Default);
        let agit_session = lineage_from_params(
            self.settlement_feature(),
            p.agent.as_deref(),
            p.expected_agent_id.as_deref(),
            p.branch.as_deref(),
        )?;

        // A durable result or ambiguous Pending intent wins over environmental
        // drift. The project may have been unbound, moved, or lost its runtime
        // after the original launch; a retry is not a new launch and must
        // return the same result (or the same explicit recovery error), not
        // rediscover a different cwd/lineage first.
        if let Some(start_id) = start_id.as_deref()
            && let Some(intent) = self.roster.starts.get(start_id).cloned()
        {
            let retry_spec = roster::StartSpec {
                workspace_id: p.workspace_id.clone(),
                project_id: p.project_id.clone(),
                runtime: p.runtime.clone(),
                cwd: intent.spec.cwd.clone(),
                agit_session: agit_session.as_ref().map(ToString::to_string),
                expected_agent_id: agit_session
                    .as_ref()
                    .map(|lineage| lineage.agent_id().to_string()),
                prompt: p.prompt.clone(),
                by: p.by.clone(),
                permission_mode: mode,
            };
            if !retry_spec.same_launch_as(&intent.spec) {
                return Err(conflicting_start_error());
            }
            match intent.state {
                roster::StartState::Completed { result } => {
                    return serde_json::to_value(result)
                        .map_err(|error| RpcError::new(ErrorCode::Internal, error.to_string()));
                }
                roster::StartState::Pending { session }
                    if self.sessions.contains_key(&session.session_id) =>
                {
                    let result = SessionStartResult {
                        start_id: Some(start_id.to_string()),
                        session,
                    };
                    self.persist_completed_start(start_id, result.clone())?;
                    return serde_json::to_value(result)
                        .map_err(|error| RpcError::new(ErrorCode::Internal, error.to_string()));
                }
                roster::StartState::Pending { session } => {
                    return Err(pending_start_error(start_id, &session.session_id));
                }
            }
        }
        if start_id.is_some() && self.roster.start_history_lost {
            return Err(lost_start_history_error());
        }
        // Re-check locally: the hub said this workspace is ours, but the hub is
        // a relay, not an authority.
        if !self.mirror.has_workspace(&p.workspace_id) {
            return Err(RpcError::new(
                ErrorCode::WorkspaceNotFound,
                format!("workspace {} is not bound on this machine", p.workspace_id),
            )
            .with_hint(
                "bind it from the web, or run `agit rc status` to see what this machine has",
            ));
        }
        let cwd = self
            .mirror
            .project_path(&p.workspace_id, &p.project_id)
            .ok_or_else(|| {
                RpcError::new(
                    ErrorCode::PathNotAllowed,
                    "that project is not bound in this workspace",
                )
            })?;
        let roots = self.mirror.roots(&p.workspace_id);
        let cwd = policy::require_within(&cwd, &roots)
            .map_err(|e| RpcError::new(ErrorCode::PathNotAllowed, e.to_string()))?;

        let session_id = crate::domain::meta::mint_session_id();
        // The create path is judged too — otherwise the owner-only gate is decoration: whoever
        // is blocked from switching permission mode just opens a **new** session carrying
        // `bypass` and walks around it. Starting a new session in a loosened mode is the same
        // act as switching a session into that mode.
        // Starting a new session: there is no "current mode" to compare against, only the
        // absolute test.
        require_owner_to_loosen(Some(caller), mode, None)?;
        // Judge that this runtime can really express it too. Launching with a mode it cannot
        // express gives the user a session that believes it is in `plan` and writes whatever it
        // likes.
        let supported = crate::rc::harness::capability_of(&p.runtime);
        if !supported.permission_modes.contains(&mode) {
            return Err(RpcError::new(
                ErrorCode::RuntimeUnavailable,
                format!("{} cannot express that permission mode", p.runtime),
            )
            .with_hint("the picker should only offer what `capabilities` reports"));
        }
        let now = chrono::Utc::now().to_rfc3339();
        let info = SessionInfo {
            session_id: session_id.clone(),
            workspace_id: p.workspace_id.clone(),
            project_id: Some(p.project_id.clone()),
            runtime: p.runtime.clone(),
            agent: agit_session.as_ref().map(|l| l.slug()),
            branch: agit_session.as_ref().map(|l| l.branch().to_string()),
            status: SessionStatus::Idle,
            last_seq: 0,
            gist: None,
            dangerous: mode.is_dangerous(),
            permission_mode: Some(mode),
            created_at: now.clone(),
            updated_at: now,
        };

        let spec = LaunchSpec {
            cwd: cwd.clone(),
            resume_from: None,
            agit_session: agit_session.clone(),
            model: None,
            dangerous: mode.is_dangerous(),
            permission_mode: Some(mode),
        };

        let Some(start_id) = start_id else {
            // Rolling compatibility: an old hub on an unnegotiated socket may
            // still launch exactly as before. It cannot send or receive keyed
            // semantics until the feature is explicitly ACKed.
            let session = self
                .spawn_session(
                    info,
                    spec,
                    danger::TranscriptDanger::fresh_transcript(),
                    frames,
                    p.prompt,
                    p.by,
                )
                .await?;
            return Ok(serde_json::to_value(SessionStartResult {
                start_id: None,
                session,
            })
            .unwrap());
        };

        let start_spec = roster::StartSpec {
            workspace_id: p.workspace_id.clone(),
            project_id: p.project_id.clone(),
            runtime: p.runtime.clone(),
            cwd: cwd.to_string_lossy().into_owned(),
            agit_session: agit_session.as_ref().map(ToString::to_string),
            expected_agent_id: agit_session
                .as_ref()
                .map(|lineage| lineage.agent_id().to_string()),
            prompt: p.prompt.clone(),
            by: p.by.clone(),
            permission_mode: mode,
        };

        match self.roster.claim_start(&start_id, start_spec, info.clone()) {
            roster::StartClaim::Completed(result) => {
                return serde_json::to_value(result)
                    .map_err(|error| RpcError::new(ErrorCode::Internal, error.to_string()));
            }
            roster::StartClaim::Pending(session) => {
                // The daemon stayed alive and still owns the exact logical
                // session: the launch succeeded but persisting Completed did
                // not. Finish that write and replay; never launch again.
                if self.sessions.contains_key(&session.session_id) {
                    let result = SessionStartResult {
                        start_id: Some(start_id.clone()),
                        session,
                    };
                    self.persist_completed_start(&start_id, result.clone())?;
                    return serde_json::to_value(result)
                        .map_err(|error| RpcError::new(ErrorCode::Internal, error.to_string()));
                }
                return Err(pending_start_error(&start_id, &session.session_id));
            }
            roster::StartClaim::Conflict => return Err(conflicting_start_error()),
            roster::StartClaim::HistoryLost => return Err(lost_start_history_error()),
            roster::StartClaim::Reserved => {}
        }

        if let Err(error) = self.roster.save() {
            self.roster.forget_start(&start_id);
            return Err(RpcError::new(
                ErrorCode::Internal,
                format!("could not durably reserve session.start before launch: {error:#}"),
            )
            .with_hint("nothing was launched; retry in a moment with the same start_id"));
        }

        let launched = match self
            .spawn_session(
                info,
                spec,
                danger::TranscriptDanger::fresh_transcript(),
                frames,
                p.prompt,
                p.by,
            )
            .await
        {
            Ok(value) => value,
            Err(failure) if !failure.reached_launch => {
                // The failure is before `Session::launch`, so **no process provably started**.
                // This case must release the reservation: keeping it parks this start_id in
                // Pending forever, and Pending is durable — one pure persistence failure would
                // scrap this launch permanently, past saving even once the disk is writable
                // again. The reservation-persist failure path above judges the same class of
                // thing.
                self.roster.forget_start(&start_id);
                if let Err(error) = self.roster.save() {
                    // Release the in-memory copy even when the release cannot be persisted:
                    // **this process** knows nothing started, so an immediate retry with the
                    // same start_id is safe. The Pending row on disk speaks again only after a
                    // restart, and it fails closed then — better that than scrapping in memory
                    // too a start_id that provably never ran.
                    eprintln!(
                        "agitd: released session.start {start_id} in memory but could not persist it: {error:#}"
                    );
                }
                return Err(failure.error);
            }
            Err(failure) => {
                // The launch crossed the OS spawn boundary (or its outcome is
                // unknown): a native process may be running right now. Keep
                // Pending forever rather than guessing a second launch is safe.
                return Err(RpcError::new(
                    ErrorCode::SessionBusy,
                    format!(
                        "session.start was durably reserved but launch completion is unknown: {}",
                        failure.error.message
                    ),
                )
                .with_hint(format!(
                    "no second launch will be attempted for {start_id}; inspect this machine and retry the same start_id after recovery"
                )));
            }
        };
        let result = SessionStartResult {
            start_id: Some(start_id.clone()),
            session: launched,
        };
        self.persist_completed_start(&start_id, result.clone())?;
        serde_json::to_value(result)
            .map_err(|error| RpcError::new(ErrorCode::Internal, error.to_string()))
    }

    fn persist_completed_start(
        &mut self,
        start_id: &str,
        result: SessionStartResult,
    ) -> Result<(), RpcError> {
        let before = self.roster.starts.get(start_id).cloned();
        self.roster
            .complete_start(start_id, result)
            .map_err(|error| RpcError::new(ErrorCode::Internal, error.to_string()))?;
        if let Err(error) = self.roster.save() {
            match before {
                Some(intent) => {
                    self.roster.starts.insert(start_id.to_string(), intent);
                }
                None => {
                    self.roster.starts.remove(start_id);
                }
            }
            return Err(RpcError::new(
                ErrorCode::SessionBusy,
                format!("the session launched but its idempotent result is not durable: {error:#}"),
            )
            .with_hint(format!(
                "retry the same start_id {start_id}; the daemon will not launch a second process"
            )));
        }
        Ok(())
    }

    /// Launch a session and register it. `spec.resume_from` is the only difference between
    /// `start_session` (create) and `resume_session` (take over one already on this machine), so
    /// they share this tail.
    ///
    /// `danger` is the verdict on that transcript and comes only from [`danger`](super::danger):
    /// this shared tail is the **only** place that stamps `SessionInfo.dangerous`, so "judged"
    /// and "the bit reported out" can never come apart — with each path copying into
    /// `SessionInfo` itself, the resume-by-logical-id path copies the value on its own row, and
    /// the same transcript turns clean when a different workspace asks.
    ///
    /// Failure comes back as [`SpawnFailure`]: keyed `session.start` needs `reached_launch` to
    /// tell "provably nothing happened" apart from "a native process may already be running".
    pub(super) async fn spawn_session(
        &mut self,
        mut info: SessionInfo,
        spec: LaunchSpec,
        danger: danger::TranscriptDanger,
        frames: &mpsc::Sender<Frame>,
        prompt: Option<String>,
        by: Option<String>,
    ) -> Result<SessionInfo, SpawnFailure> {
        // **Resuming a transcript requires having judged it and having judged whoever asks for
        // it.**
        //
        // Two slips do not pass this check: `TranscriptDanger::fresh_transcript()` ("this run
        // continues no existing transcript", true only of a freshly started conversation) and
        // the one `danger::judge()` produces (for read-only following; it judges no caller).
        // Either of them together with `--resume` means someone added a resume path and missed
        // one of the steps — letting it through loads context that may have run unchecked into a
        // session reported as clean, and from then on every `turn.start` / `turn.steer` /
        // `approval.decide` passes the `Need::Drive` gate. Better this launch fails.
        if spec.resume_from.is_some() && !danger.cleared_a_transcript() {
            return Err(SpawnFailure::before_launch(RpcError::new(
                ErrorCode::Internal,
                "this launch resumes a harness transcript that was never cleared for this caller",
            )));
        }
        danger::stamp(&mut info, danger);
        let session_id = info.session_id.clone();
        // Capture only ambiguity inherited by this new harness generation.
        // A same-generation turn can arm another token after spawn; Ready must
        // never clear that newer attempt merely because its event was delayed.
        let restart_guard_attempts: std::collections::BTreeSet<String> = self
            .roster
            .sessions
            .get(&session_id)
            .map(|entry| entry.guard_attempts.keys().cloned().collect())
            .unwrap_or_default();
        let restart_guard_mode =
            (!restart_guard_attempts.is_empty()).then(|| spec.effective_mode());
        if restart_guard_mode.is_some_and(|mode| mode != crate::protocol::PermissionMode::Plan) {
            return Err(SpawnFailure::before_launch(RpcError::new(
                ErrorCode::Internal,
                "a session with unresolved turn guards was not prepared for a Plan restart",
            )));
        }
        // A session starting in a dangerous mode **persists the monotonic danger bit before the
        // harness gets anything** — the same invariant as `arm_danger_before_loosening`, guarding
        // here the "dangerous from birth" path (start-at-bypass, and takeover under history_lost
        // or a poisoned state).
        //
        // Writing the durable record only in `SessionNote::Bound` is too late: for claude that is
        // an async stretch after launch, for codex not until native Ready, and a `save()` failure
        // there is only an eprintln. If agitd crashes inside that window, the disk holds no trace
        // that this session ran without checks — whoever then takes it over by harness thread id
        // finds nothing through `logical_for_thread` and mints a clean `ever_dangerous == false`
        // identity that any operator can drive, picking up everything that unchecked run read
        // into its context.
        //
        // The thread id may not exist yet (codex waits for Ready), so an empty string holds the
        // danger bit's place; when `Bound` arrives, `record` fills in the real id and keeps the
        // monotonic bit. A failed launch **does not delete this row** either: the launch may
        // already have crossed the OS spawn boundary (the same treatment keyed start gives
        // Pending), and deleting it bets that process never existed.
        //
        // The session also goes into `unconfirmed_dangerous_bindings`, persisted in the **same
        // save**. An empty thread covers only the "the native id is not born yet" half; the other
        // half is that the id **changes** — claude's slow-path resume swaps in a new id at
        // `system/init` and the transcript file follows, while the ledger still holds the old
        // one. `Roster::dangerous_start_unaccounted` collects both halves, folding takeover and
        // following of an unknown thread on this ground into owner-only until `Bound` really
        // persists.
        if info.dangerous {
            let inserted = self.roster.get(&session_id).is_none();
            if inserted {
                self.roster.record(
                    &session_id,
                    roster::Entry {
                        runtime: info.runtime.clone(),
                        thread_id: spec.resume_from.clone().unwrap_or_default(),
                        cwd: spec.cwd.to_string_lossy().into_owned(),
                        workspace_id: info.workspace_id.clone(),
                        project_id: info.project_id.clone(),
                        agit_session: spec.agit_session.as_ref().map(ToString::to_string),
                        expected_agent_id: spec
                            .agit_session
                            .as_ref()
                            .map(|lineage| lineage.agent_id().to_string()),
                        permission_mode: info.permission_mode,
                        guard_attempts: Default::default(),
                        prior_threads: vec![],
                        ever_dangerous: true,
                    },
                );
            }
            // This row is **already** in the ledger (the resume-by-logical-id path lands here):
            // merge the judged bit into it, persisted before launch just the same.
            //
            // Without this step the row in the ledger and the session in front of it say two
            // different things — `ever_dangerous` is false while `info.dangerous` is true (that
            // bit is judged from the transcript: a sibling row for the same transcript in
            // another workspace was poisoned). The consequence is not "the display disagrees":
            // `arm_danger_before_loosening` uses this row to judge whether this mode switch is
            // what **newly** turned it dangerous, and answering "yes" means that once the switch
            // is proven not to have run, `disarm_danger` turns it back to false — washing a
            // transcript that really did run unchecked into a clean one. The monotonic bit only
            // goes false → true.
            let upgraded = !inserted
                && self
                    .roster
                    .sessions
                    .get_mut(&session_id)
                    .is_some_and(|entry| !std::mem::replace(&mut entry.ever_dangerous, true));
            let armed = self.roster.arm_unconfirmed_binding(&session_id);
            if (inserted || upgraded || armed)
                && let Err(error) = self.roster.save()
            {
                // No launch if it cannot be persisted. Better this start fails than releasing
                // a session that "runs without checks and leaves no record on disk" — one crash
                // brings it back with a clean identity. Each of the three flags records what
                // this pass actually changed, which is what makes the rollback complete.
                if inserted {
                    self.roster.sessions.remove(&session_id);
                }
                if upgraded && let Some(entry) = self.roster.sessions.get_mut(&session_id) {
                    entry.ever_dangerous = false;
                }
                if armed {
                    self.roster.confirm_binding(&session_id);
                }
                return Err(SpawnFailure::before_launch(
                    RpcError::new(
                        ErrorCode::Internal,
                        format!(
                            "could not durably record that this session starts without permission checks: {error:#}"
                        ),
                    )
                    .with_hint("nothing was launched; check that ~/.agit/rc is writable and retry"),
                ));
            }
        }
        // This stream is alive again: reopen the replay buffer.
        //
        // `SessionNote::Ended` closes it (a finished session holding up to 8192 frames nobody
        // picks up), and a logical session can come back to life — without reopening, for the
        // whole lifetime of this resumed stretch `session.subscribe` backfills nothing, and
        // every viewer reconnect depends on it.
        self.journal.resume(&session_id);
        let cwd = spec.cwd.clone();
        let agit_session = spec.agit_session.clone();
        let resume_from = spec.resume_from.clone();

        // Allocate provenance before the bridge exists: no frame may enter the
        // shared daemon queue without the exact generation that produced it.
        // A failed launch consumes the number but never materializes its
        // tombstone, so any frame it managed to enqueue is rejected later.
        self.session_generation += 1;
        let generation = self.session_generation;

        // Stamp every frame this session emits with the stream id and the local generation.
        // Both are overwritten unconditionally: the supervisor does not own the logical stream,
        // and the JSON does not own generation provenance.
        let (tagged_tx, mut tagged_rx) = mpsc::channel::<Frame>(1024);
        {
            let frames = frames.clone();
            let sid = session_id.clone();
            tokio::spawn(async move {
                while let Some(mut f) = tagged_rx.recv().await {
                    tag_session_frame(&mut f, &sid, generation);
                    if frames.send(f).await.is_err() {
                        break;
                    }
                }
            });
        }

        let confinement = self.confinement_for(&info.workspace_id);
        let session = Session::launch(
            info.clone(),
            spec,
            tagged_tx,
            self.notes.clone(),
            confinement,
            self.settlement.subscribe(),
            generation,
            self.secret_filter.clone(),
        )
        .await
        .map_err(|failure| {
            // The spawn boundary **is not this line**: `Session::launch` goes all the way down
            // to `Proc::spawn` before it really calls `Command::spawn`, and before that it
            // returns just as well for "the executable is not on PATH", "no execute bit", "the
            // cwd does not exist", "the process-tree fence cannot be built" — each of which
            // proves not one harness started. Accounting for all of them as "crossed" parks a
            // request carrying a start key in Pending forever: a retry gets
            // `pending_start_error`, and the Pending row on disk outlives a daemon restart while
            // no process exists on the machine at all.
            //
            // So the answer comes from the layer that really crosses that boundary
            // ([`harness::proc::LaunchError`]); this only translates it.
            let reached_spawn = failure.reached_spawn();
            let error = RpcError::new(ErrorCode::RuntimeUnavailable, failure.to_string());
            if reached_spawn {
                SpawnFailure::after_launch(error)
            } else {
                SpawnFailure::before_launch(error.with_hint(
                    "nothing was launched on this machine; fix the runtime and retry the same start_id",
                ))
            }
        })?;
        // A completed `Session::launch` is proof the native harness exists.
        // Never register a generation that failed to create one, and never
        // erase this entry when its `Live` handle later ends.
        self.latest_session_generations
            .insert(session_id.clone(), generation);

        // The harness's own id and the local lineage registration **are not done here**.
        //
        // A codex thread id does not exist until `thread/started`, so reading it at this moment
        // is necessarily `None` — taking that as the answer permanently skips landing, the
        // roster and the double-writer guard for every new codex session, and a conversation
        // held on the web settles no commit. The supervisor reports it through
        // `SessionNote::Bound` at the moment the id is knowable (launch for claude, Ready for
        // codex), retrying before every turn settlement on failure. This keeps only the one
        // resume already knows, as the initial value.
        let runtime_thread_id = resume_from;
        let _ = &cwd;
        let _ = &agit_session;

        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>(64);
        let task = tokio::spawn(session.run(cmd_rx));

        let bootstrap_tx = cmd_tx.clone();
        let claude_restart_guard_barrier =
            needs_claude_restart_guard_barrier(&info.runtime, &restart_guard_attempts);

        self.sessions.insert(
            session_id.clone(),
            Live {
                generation,
                task,
                danger_arm: 0,
                pending_mode: None,
                approval_session_modes: HashMap::new(),
                rpc_gate: Arc::new(Mutex::new(())),
                rpc_guard_sensitive: false,
                confirmed_turn_guards: Default::default(),
                inflight_turn_guard: None,
                restart_guard_attempts,
                restart_guard_mode,
                ended: false,
                info: info.clone(),
                tx: cmd_tx,
                runtime_thread_id,
            },
        );

        // Queue Claude's recovery evidence only after the generation is in
        // `Live`. The supervisor consumes this marker from inside its command
        // loop and waits for the ACKed durable save before it can consume the
        // following creation prompt or any later viewer command. Codex keeps
        // using its native Ready notification instead.
        if claude_restart_guard_barrier
            && bootstrap_tx
                .send(Command::ClaudeRestartGuardReady)
                .await
                .is_err()
        {
            self.detach_failed_session_generation(&session_id, generation);
            return Err(SpawnFailure::after_launch(RpcError::new(
                ErrorCode::RuntimeUnavailable,
                "the Claude recovery supervisor ended before its Plan barrier",
            )));
        }
        if let Some(prompt) = prompt {
            let _ = bootstrap_tx
                .send(Command::InitialTurn {
                    message: prompt,
                    by,
                })
                .await;
        }
        // Stamp the current seq watermark. See [`Daemon::stamped`]: the web needs it to order a
        // response carrying no seq against the event stream, or an `ended` from yesterday locks
        // the input box forever.
        Ok(self.stamped(info))
    }
}
