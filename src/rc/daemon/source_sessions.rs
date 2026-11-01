//! Native source coordinates are resolved locally before sharing an executor.

use super::*;
use crate::protocol::NativeSourceRef;
use crate::rc::runtime_context::RuntimeContext;
use crate::rc::runtime_sources::Registry;

pub(super) fn unavailable(error: impl std::fmt::Display) -> RpcError {
    RpcError::new(
        ErrorCode::SessionNotFound,
        format!("native conversation is unavailable: {error}"),
    )
}

/// The display labels the source catalog holds for a native conversation, if it has scanned it,
/// protected as the catalog protects them.
///
/// A source-bound session that opens without them reaches clients and the hub with no name, and
/// the hub keeps a missing label until something reports one. The hub also keeps the first label
/// it is given, so one whose check does not pass is left out, never sent as read.
pub(super) fn catalog_labels(
    source: &NativeSourceRef,
    native: &str,
) -> (Option<String>, Option<String>) {
    crate::rc::runtime_catalog::Catalog::open()
        .and_then(|catalog| catalog.lookup(&source.session_ref(native)))
        .ok()
        .flatten()
        .filter(|row| row.source_id == source.source_id && row.native_session_id == native)
        .map_or((None, None), |mut row| {
            protect_catalog_labels(std::slice::from_mut(&mut row));
            (row.title, row.gist)
        })
}

/// A transcript still being restored is a busy conversation that clients retry, not a missing one.
pub(super) fn locate_failed(error: anyhow::Error) -> RpcError {
    if error.is::<crate::adapter::codex_rollout::Restoring>() {
        let mut error = RpcError::new(
            ErrorCode::SessionBusy,
            "conversation history is being prepared; retry shortly",
        );
        error.data = Some(serde_json::json!({"retryable": true, "reason": "transcript_restoring"}));
        return error;
    }
    unavailable(error)
}

/// The refusal of a non-owner's source resume whose thread's current mode is not already Default
/// or Plan, or cannot be read now.
fn native_mode_owner_only() -> RpcError {
    classified(
        RpcError::new(
            ErrorCode::Forbidden,
            "this conversation is set to act without asking; only its owner may resume it",
        )
        .with_hint("set it to Default or Plan in the native client, or ask the owner to resume it"),
        false,
        "native_mode_owner_only",
    )
}

impl Daemon {
    pub(super) fn prepare_source_resume(
        &mut self,
        p: SessionResume,
        caller: &crate::protocol::CallerClaim,
        frames: &mpsc::Sender<Frame>,
        identity: (&str, &str),
        prior: Option<roster::Entry>,
        authority: &crate::rc::authority::Guard,
    ) -> Result<SessionOpening, RpcError> {
        let (source_id, native) = identity;
        require_same_workspace(caller, &p.session_id, &p.workspace_id)?;
        if prior
            .as_ref()
            .is_some_and(|entry| entry.workspace_id != p.workspace_id)
        {
            return Err(unavailable("conversation belongs to another workspace"));
        }
        let registry = Registry::open().map_err(unavailable)?;
        let context = RuntimeContext::resolve(&registry, source_id).map_err(unavailable)?;
        let source = NativeSourceRef {
            source_id: source_id.into(),
            generation: context.source.generation,
        };
        let key = crate::domain::link::NativeBinding {
            source: source.clone(),
            thread_id: native.into(),
        }
        .key()
        .map_err(unavailable)?;
        let roots = self.mirror.roots(&p.workspace_id);
        let thread = context
            .locate_ready(native, &roots)
            .map_err(locate_failed)?;
        crate::rc::local_goal::validate_header(&thread.transcript, native, &thread.cwd)
            .map_err(unavailable)?;
        if prior
            .as_ref()
            .is_some_and(|entry| std::path::Path::new(&entry.cwd) != thread.cwd)
        {
            return Err(unavailable(
                "conversation directory differs from its registered binding",
            ));
        }
        let project = self
            .mirror
            .workspaces
            .get(&p.workspace_id)
            .into_iter()
            .flat_map(|projects| projects.keys())
            .filter(|id| {
                p.project_id
                    .as_ref()
                    .is_none_or(|requested| requested == *id)
            })
            .filter_map(|id| {
                self.mirror
                    .project_path(&p.workspace_id, id)
                    .filter(|root| thread.cwd.starts_with(root))
                    .map(|root| (id.clone(), root))
            })
            .max_by_key(|(_, root)| root.components().count())
            .map(|(id, _)| id)
            .ok_or_else(|| unavailable("project binding is unavailable"))?;
        let project_path = self
            .mirror
            .project_path(&p.workspace_id, &project)
            .ok_or_else(|| unavailable("project binding changed"))?;
        authority.check_project(&project, &project_path)?;
        authority.check()?;
        // A non-owner is judged only by the mode the index records now; one that cannot be read
        // now, or only from the rollout's history, leaves the resume to the owner.
        let native_mode = if caller.is_owner() {
            context
                .settings(&thread)
                .map_err(unavailable)?
                .permission_mode
        } else {
            Some(
                context
                    .current_mode(&thread)
                    .ok()
                    .flatten()
                    .ok_or_else(native_mode_owner_only)?,
            )
        };
        let danger = danger::authorize_source(
            &self.roster,
            caller,
            source_id,
            native,
            &thread.cwd.to_string_lossy(),
            &thread.transcript,
            native_mode,
        )?;
        // A shared native thread resumes in the mode it is set to; the request cannot tighten it.
        // So a non-owner may resume it only where that mode is already no looser than Default
        // (Default or Plan), and the session opens in exactly that mode; Auto or Accept edits stay
        // the owner's. A private fallback launch takes the same mode from the spec.
        let mode = danger::resume_mode(caller, native_mode);
        if mode != native_mode {
            return Err(native_mode_owner_only());
        }
        let logical = if prior.is_some() {
            p.session_id.clone()
        } else {
            self.roster
                .logical_for_thread_in("codex", Some(source_id), native, &p.workspace_id)
                .unwrap_or_else(crate::domain::meta::mint_session_id)
        };
        if self.ending_in(&logical, caller) {
            return Err(classified(
                RpcError::new(
                    ErrorCode::SessionBusy,
                    "this conversation is still settling",
                ),
                true,
                "settling",
            ));
        }
        if let Some(info) = self.supervised_in(&logical, caller) {
            return Ok(SessionOpening::Ready(
                serde_json::to_value(SessionResumeResult {
                    session: self.stamped(info),
                    launched: None,
                })
                .unwrap(),
            ));
        }
        // Nothing here runs this conversation. Resuming it loads it into a Codex server, even one
        // already listening for a terminal that has it open, and that starts it: `attach_only`
        // never joins a conversation open only in a terminal.
        super::sessions::attach_only_refusal(p.attach_only, "codex", native, &thread.cwd)?;
        // A resume outranks recovery of the same conversation.
        self.preempt_archive(&logical, "codex", Some(&source), Some(native));
        let prior = prior.or_else(|| self.roster.get(&logical).cloned());
        if prior
            .as_ref()
            .is_some_and(|entry| !entry.guard_attempts.is_empty())
            && native_mode != Some(crate::protocol::PermissionMode::Plan)
        {
            return Err(classified(
                RpcError::new(
                    ErrorCode::SessionBusy,
                    "native permissions are unresolved; select Plan in the native client before reconnecting",
                ),
                false,
                "native_permissions_unresolved",
            ));
        }
        let store = crate::domain::store::Store::open().map_err(unavailable)?;
        if let Some(store) = store {
            let existing = crate::domain::link::read_archive_link_snapshot(&store, "codex", &key)
                .map_err(unavailable)?;
            if existing
                .as_ref()
                .is_some_and(|snapshot| !snapshot.link.is_active())
            {
                return Err(unavailable(
                    "native instance was superseded; resume the active branch",
                ));
            }
        }
        let lineage = match &prior {
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
        let now = chrono::Utc::now().to_rfc3339();
        let (title, gist) = catalog_labels(&source, native);
        let info = SessionInfo {
            interrupt_fenced: None,
            publication: None,
            session_id: logical,
            native_source: Some(source),
            runtime_session_id: Some(native.into()),
            workspace_id: p.workspace_id.clone(),
            project_id: Some(project),
            runtime: "codex".into(),
            agent: lineage.as_ref().map(|lineage| lineage.slug()),
            branch: lineage.as_ref().map(|lineage| lineage.branch().to_string()),
            status: SessionStatus::Idle,
            last_seq: 0,
            gist,
            title,
            labels_withheld: None,
            dangerous: false,
            permission_mode: mode,
            created_at: now.clone(),
            updated_at: now,
        };
        let spec = LaunchSpec {
            cwd: thread.cwd,
            resume_from: Some(native.into()),
            agit_session: lineage,
            model: None,
            dangerous: false,
            permission_mode: mode,
            attach_only: p.attach_only,
        };
        context.validate().map_err(unavailable)?;
        let spawn = self.prepare_spawn(
            info,
            spec,
            danger,
            frames,
            p.prompt,
            MessageAttribution::from_caller(caller, p.by, None),
        )?;
        Ok(SessionOpening::launch(spawn, OpeningReply::Resume))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn source_resume_preserves_identity_and_checks_roles_binding_and_revocation() {
        let root = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(root.path(), || {
            let project = root.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let project = project.canonicalize().unwrap();
            let native = uuid::Uuid::new_v4().to_string();
            let registry = Registry::open().unwrap();
            let mut sources = vec![];
            let mut databases = vec![];
            for name in ["alpha", "beta"] {
                let home = root.path().join(name);
                std::fs::create_dir(&home).unwrap();
                let transcript = home.join(format!("rollout-{native}.jsonl"));
                std::fs::write(
                    &transcript,
                    format!(
                        "{}\n{}\n",
                        serde_json::json!({
                            "type":"session_meta", "payload":{"id":native,"cwd":project}
                        }),
                        serde_json::json!({"type":"turn_context","payload":{
                            "approval_policy":"never","sandbox_policy":{"type":"read-only"}}})
                    ),
                )
                .unwrap();
                let db = rusqlite::Connection::open(home.join("state_1.sqlite")).unwrap();
                db.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER, archived INTEGER, model TEXT, reasoning_effort TEXT, approval_mode TEXT, sandbox_policy TEXT)").unwrap();
                db.execute("INSERT INTO threads VALUES (?1,?2,?3,'question','cli',1,0,'fixture','high','never','{\"type\":\"read-only\"}')",
                    rusqlite::params![native, transcript.to_str(), project.to_str()]).unwrap();
                sources.push(registry.register(&home, None, None, None).unwrap());
                databases.push(db);
            }
            let daemon = super::super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
            let mut state = daemon.try_lock().unwrap();
            state.opts.local_owner = true;
            state.mirror.bind("ws", "project", &project).unwrap();
            let (frames, _) = mpsc::channel(16);
            let claim = |role: &str| crate::protocol::CallerClaim {
                account_id: Some("member".into()),
                username: None,
                role: role.into(),
                workspace_id: "ws".into(),
            };
            let request = |source: &crate::rc::runtime_sources::RuntimeSource| SessionResume {
                project_id: None,
                workspace_id: "ws".into(),
                session_id: source.session_ref(&native),
                prompt: None,
                by: None,
                agent: None,
                expected_agent_id: None,
                branch: None,
                attach_only: false,
            };
            // A conversation opens under the name the catalog shows for it, not unnamed.
            let catalog = crate::rc::runtime_catalog::Catalog::open().unwrap();
            let roots = policy::CanonicalRoots::from_untrusted([project.clone()]);
            for _ in &sources {
                catalog.reconcile(&roots).unwrap();
            }
            let mut ids = vec![];
            let mut infos = vec![];
            for source in &sources {
                let opening = state
                    .prepare_source_resume(
                        request(source),
                        &claim("operator"),
                        &frames,
                        (&source.source_id, &native),
                        None,
                        &Default::default(),
                    )
                    .unwrap();
                let SessionOpening::Launch(spawn, _) = opening else {
                    panic!("source must attach");
                };
                assert_eq!(spawn.spec.resume_from.as_deref(), Some(native.as_str()));
                assert!(spawn.spec.agit_session.is_none());
                assert_eq!(
                    spawn.info.native_source.as_ref().unwrap().source_id,
                    source.source_id
                );
                assert_eq!(
                    spawn.info.permission_mode,
                    Some(crate::protocol::PermissionMode::Plan)
                );
                assert!(!spawn.info.dangerous);
                assert_eq!(spawn.info.gist.as_deref(), Some("question"));
                ids.push(spawn.info.session_id.clone());
                infos.push(spawn.info);
            }
            assert_ne!(ids[0], ids[1]);
            assert_eq!(state.opening_sessions.len(), 2);
            state.opening_sessions.clear();
            // A source conversation is loaded into a Codex server to resume it, so `attach_only`
            // refuses it, for the owner too, and reserves nothing.
            let error = state
                .prepare_source_resume(
                    SessionResume {
                        attach_only: true,
                        ..request(&sources[0])
                    },
                    &claim("owner"),
                    &frames,
                    (&sources[0].source_id, &native),
                    None,
                    &Default::default(),
                )
                .err()
                .unwrap();
            assert_eq!(
                error.data,
                Some(serde_json::json!({"retryable": false, "reason": "not_running"}))
            );
            assert!(state.opening_sessions.is_empty());
            // Once the daemon runs it, the same `attach_only` resume joins that session, and the
            // reply does not say it launched anything. A running session has its roster row,
            // which is how a resume finds its logical id.
            state.roster.sessions.insert(
                ids[0].clone(),
                serde_json::from_value(serde_json::json!({
                    "native_source": infos[0].native_source,
                    "runtime": "codex",
                    "thread_id": native,
                    "cwd": project,
                    "workspace_id": "ws",
                    "project_id": "project",
                }))
                .unwrap(),
            );
            let (tx, _rx) = mpsc::channel(1);
            let mut live = super::super::tests::rpc_test_live(
                &ids[0],
                1,
                tx,
                crate::protocol::PermissionMode::Plan,
            );
            live.info = infos[0].clone();
            state.sessions.insert(ids[0].clone(), live);
            let opening = state
                .prepare_source_resume(
                    SessionResume {
                        attach_only: true,
                        ..request(&sources[0])
                    },
                    &claim("owner"),
                    &frames,
                    (&sources[0].source_id, &native),
                    None,
                    &Default::default(),
                )
                .unwrap_or_else(|error| panic!("{}", error.message));
            let SessionOpening::Ready(value) = opening else {
                panic!("a running session is joined, not launched");
            };
            let joined = serde_json::from_value::<SessionResumeResult>(value).unwrap();
            assert_eq!(joined.session.session_id, ids[0]);
            assert_eq!(joined.launched, None);
            assert!(state.opening_sessions.is_empty());
            state.sessions.clear();
            state.roster.sessions.remove(&ids[0]);
            databases[0]
                .execute(
                    "UPDATE threads SET sandbox_policy='{\"type\":\"danger-full-access\"}'",
                    [],
                )
                .unwrap();
            let error = state
                .prepare_source_resume(
                    request(&sources[0]),
                    &claim("operator"),
                    &frames,
                    (&sources[0].source_id, &native),
                    None,
                    &Default::default(),
                )
                .err()
                .unwrap();
            assert_eq!(error.code, ErrorCode::DangerousSessionLocked as i32);
            assert!(state.opening_sessions.is_empty());
            databases[0]
                .execute(
                    "UPDATE threads SET sandbox_policy='{\"type\":\"externalSandbox\"}'",
                    [],
                )
                .unwrap();
            let opening = state
                .prepare_source_resume(
                    request(&sources[0]),
                    &claim("owner"),
                    &frames,
                    (&sources[0].source_id, &native),
                    None,
                    &Default::default(),
                )
                .unwrap();
            let SessionOpening::Launch(spawn, _) = opening else {
                panic!("an authorized unknown native policy remains attachable");
            };
            assert!(spawn.info.permission_mode.is_none());
            assert!(spawn.info.dangerous);
            state.opening_sessions.clear();
            struct OtherProject;
            impl crate::rc::authority::Authority for OtherProject {
                fn admit(&self, accept: &mut dyn FnMut() -> bool) -> bool {
                    accept()
                }
                fn project(&self) -> Option<(&str, &std::path::Path)> {
                    Some(("different", std::path::Path::new("/different")))
                }
            }
            let error = state
                .prepare_source_resume(
                    request(&sources[1]),
                    &claim("owner"),
                    &frames,
                    (&sources[1].source_id, &native),
                    None,
                    &crate::rc::authority::Guard::new(OtherProject),
                )
                .err()
                .unwrap();
            assert_eq!(error.code, ErrorCode::Forbidden as i32);
            assert!(state.opening_sessions.is_empty());
            registry.remove(&sources[1].source_id).unwrap();
            assert!(
                state
                    .prepare_source_resume(
                        request(&sources[1]),
                        &claim("owner"),
                        &frames,
                        (&sources[1].source_id, &native),
                        None,
                        &Default::default()
                    )
                    .is_err()
            );
            assert!(state.opening_sessions.is_empty());
        });
    }

    /// A Codex conversation of one native source in `project`, whose rollout records `turns`
    /// (approval, sandbox type) and whose index records `index` as its current settings.
    fn source_conversation(
        root: &std::path::Path,
        project: &std::path::Path,
        native: &str,
        turns: &[(&str, &str)],
        index: (&str, &str),
    ) -> crate::rc::runtime_sources::RuntimeSource {
        let home = root.join(format!("home-{native}"));
        std::fs::create_dir(&home).unwrap();
        // Where Codex keeps it, so the conversation is found without its index too.
        let day = home.join("sessions/2026/11/01");
        std::fs::create_dir_all(&day).unwrap();
        let transcript = day.join(format!("rollout-2026-11-01T00-00-00-{native}.jsonl"));
        let mut lines = vec![
            serde_json::json!({"type":"session_meta","payload":{"id":native,"cwd":project}})
                .to_string(),
        ];
        for (approval, sandbox) in turns {
            lines.push(
                serde_json::json!({"type":"turn_context","payload":{
                    "approval_policy":approval,"sandbox_policy":{"type":sandbox}}})
                .to_string(),
            );
        }
        std::fs::write(&transcript, lines.join("\n") + "\n").unwrap();
        let db = rusqlite::Connection::open(home.join("state_1.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, cwd TEXT, first_user_message TEXT, thread_source TEXT, updated_at_ms INTEGER, archived INTEGER, model TEXT, reasoning_effort TEXT, approval_mode TEXT, sandbox_policy TEXT)").unwrap();
        db.execute(
            "INSERT INTO threads VALUES (?1,?2,?3,'question','cli',1,0,'fixture','high',?4,?5)",
            rusqlite::params![
                native,
                transcript.to_str(),
                project.to_str(),
                index.0,
                serde_json::json!({"type": index.1}).to_string()
            ],
        )
        .unwrap();
        Registry::open()
            .unwrap()
            .register(&home, None, None, None)
            .unwrap()
    }

    /// Pins that a source-scoped Codex conversation is judged by its rollout's whole history and
    /// resumes for a non-owner in no looser a mode than Default. A rollout with an earlier turn
    /// run with full access is owner-only, by resume and by native message, although it is set
    /// back to Default now. One whose native mode acts without asking (Auto: never with
    /// workspace-write) is owner-only to resume, since a shared thread keeps its native mode;
    /// the owner's resume opens it in Auto. One set to Default opens for an operator in Default.
    /// A source path that judged only the ledger and the current mode, or kept the native mode
    /// for every caller, let the operator through.
    #[tokio::test]
    async fn a_source_conversation_is_judged_by_its_history_and_resumes_no_looser_than_default() {
        let root = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(root.path(), || {
            let project = root.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let project = project.canonicalize().unwrap();
            let earlier = uuid::Uuid::new_v4().to_string();
            let auto = uuid::Uuid::new_v4().to_string();
            let default = uuid::Uuid::new_v4().to_string();
            let sources = [
                source_conversation(
                    root.path(),
                    &project,
                    &earlier,
                    &[
                        ("never", "danger-full-access"),
                        ("on-request", "workspace-write"),
                    ],
                    ("on-request", "workspace-write"),
                ),
                source_conversation(
                    root.path(),
                    &project,
                    &auto,
                    &[("never", "workspace-write")],
                    ("never", "workspace-write"),
                ),
                source_conversation(
                    root.path(),
                    &project,
                    &default,
                    &[("on-request", "workspace-write")],
                    ("on-request", "workspace-write"),
                ),
            ];
            let roots = policy::CanonicalRoots::from_untrusted([project.clone()]);
            let catalog = crate::rc::runtime_catalog::Catalog::open().unwrap();
            for _ in &sources {
                catalog.reconcile(&roots).unwrap();
            }
            let daemon = super::super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
            let mut state = daemon.try_lock().unwrap();
            state.opts.local_owner = true;
            state.mirror.bind("ws", "project", &project).unwrap();
            let (frames, _) = mpsc::channel(16);
            let claim = |role: &str| crate::protocol::CallerClaim {
                account_id: Some("member".into()),
                username: None,
                role: role.into(),
                workspace_id: "ws".into(),
            };
            let mut resume =
                |source: &crate::rc::runtime_sources::RuntimeSource, native: &str, role: &str| {
                    let result = state.prepare_source_resume(
                        SessionResume {
                            project_id: None,
                            workspace_id: "ws".into(),
                            session_id: source.session_ref(native),
                            prompt: None,
                            by: None,
                            agent: None,
                            expected_agent_id: None,
                            branch: None,
                            attach_only: false,
                        },
                        &claim(role),
                        &frames,
                        (&source.source_id, native),
                        None,
                        &Default::default(),
                    );
                    state.opening_sessions.clear();
                    result.map(|opening| match opening {
                        SessionOpening::Launch(spawn, _) => {
                            (spawn.info.permission_mode, spawn.spec.permission_mode)
                        }
                        SessionOpening::Ready(_) => panic!("nothing runs this conversation yet"),
                    })
                };
            use crate::protocol::PermissionMode as M;

            let error = resume(&sources[0], &earlier, "operator").err().unwrap();
            assert_eq!(
                error.code,
                ErrorCode::DangerousSessionLocked as i32,
                "{}",
                error.message
            );
            assert!(resume(&sources[0], &earlier, "owner").is_ok());

            let error = resume(&sources[1], &auto, "operator").err().unwrap();
            assert_eq!(error.code, ErrorCode::Forbidden as i32, "{}", error.message);
            assert_eq!(
                error.data.as_ref().unwrap()["reason"],
                "native_mode_owner_only"
            );
            assert_eq!(
                resume(&sources[1], &auto, "owner").unwrap(),
                (Some(M::Auto), Some(M::Auto))
            );

            assert_eq!(
                resume(&sources[2], &default, "operator").unwrap(),
                (Some(M::Default), Some(M::Default))
            );

            let enqueue = |source: &crate::rc::runtime_sources::RuntimeSource,
                           native: &str,
                           role: &str| {
                let mut frame = Frame::request(
                    method::SESSION_ENQUEUE,
                    serde_json::json!({"workspace_id":"ws","session_id":source.session_ref(native),
                        "client_msg_id":uuid::Uuid::new_v4().to_string(),"message":"hello"}),
                );
                frame.caller = Some(claim(role));
                let watch = super::super::source_watch::SourceWatch::resolve(
                    &source.session_ref(native),
                    &roots,
                )
                .unwrap()
                .expect("the source conversation resolves");
                state
                    .prepare_inbox_target(&frame, None, Some(watch))
                    .err()
                    .map(|error| error.code)
            };
            assert_eq!(
                enqueue(&sources[0], &earlier, "operator"),
                Some(ErrorCode::DangerousSessionLocked as i32)
            );
            assert_ne!(
                enqueue(&sources[0], &earlier, "owner"),
                Some(ErrorCode::DangerousSessionLocked as i32)
            );
        });
    }

    /// Pins that a non-owner's turns into a Codex conversation of a native source re-read the
    /// thread's mode every time. Admitted in Default, the operator drives it while the native
    /// thread stays in Default; once a native client switches it to Auto, the operator is refused
    /// with `native_mode_changed` and the owner still drives it; and a mode that cannot be read
    /// refuses the operator too. A daemon that judged the mode only at attach let the operator
    /// drive a thread loosened after it.
    #[tokio::test]
    async fn a_non_owners_turn_rereads_the_native_mode() {
        let root = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(root.path(), || {
            let project = root.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let project = project.canonicalize().unwrap();
            let native = uuid::Uuid::new_v4().to_string();
            let source = source_conversation(
                root.path(),
                &project,
                &native,
                &[("on-request", "workspace-write")],
                ("on-request", "workspace-write"),
            );
            let roots = policy::CanonicalRoots::from_untrusted([project.clone()]);
            crate::rc::runtime_catalog::Catalog::open()
                .unwrap()
                .reconcile(&roots)
                .unwrap();
            let (tx, _rx) = mpsc::channel(1);
            let mut live = super::super::tests::rpc_test_live(
                "agit-source",
                1,
                tx,
                crate::protocol::PermissionMode::Default,
            );
            live.info.runtime = "codex".into();
            live.info.workspace_id = "ws".into();
            live.info.native_source = Some(NativeSourceRef {
                source_id: source.source_id.clone(),
                generation: source.generation,
            });
            live.runtime_thread_id = Some(native.clone());
            live.admitted_mode = Some(crate::protocol::PermissionMode::Default);
            let daemon = super::super::tests::rpc_test_daemon(
                HashMap::from([("agit-source".to_owned(), live)]),
                Roster::default(),
            );
            let mut state = daemon.try_lock().unwrap();
            state.mirror.bind("ws", "project", &project).unwrap();
            let claim = |role: &str| crate::protocol::CallerClaim {
                account_id: Some("member".into()),
                username: None,
                role: role.into(),
                workspace_id: "ws".into(),
            };
            let drive = |role: &str| {
                state
                    .session_channel("agit-source", &claim(role), Need::Drive)
                    .err()
                    .map(|error| (error.code, error.data))
            };
            assert_eq!(drive("operator"), None);
            let db = rusqlite::Connection::open(
                root.path().join(format!("home-{native}/state_1.sqlite")),
            )
            .unwrap();
            db.execute(
                "UPDATE threads SET approval_mode='never', sandbox_policy='{\"type\":\"workspace-write\"}'",
                [],
            )
            .unwrap();
            let changed = Some((
                ErrorCode::Forbidden as i32,
                Some(serde_json::json!({"retryable": false, "reason": "native_mode_changed"})),
            ));
            assert_eq!(drive("operator"), changed);
            assert_eq!(drive("owner"), None);
            db.execute(
                "UPDATE threads SET approval_mode='on-request', sandbox_policy='{\"type\":\"externalSandbox\"}'",
                [],
            )
            .unwrap();
            assert_eq!(drive("operator"), changed);
            assert_eq!(drive("owner"), None);
        });
    }

    /// Pins the mode hold on a native message to a source Codex conversation, through the real
    /// gate (`prepare_inbox_target` with a resolved source watch). With the thread in Auto, an
    /// operator's message is refused with `native_mode_changed` and the owner's is not; with the
    /// thread in Default, the operator's is not refused either. A gate that dropped the hold let an
    /// operator message a thread set to act without asking.
    #[tokio::test]
    async fn a_native_message_to_a_source_thread_holds_to_default_for_non_owners() {
        let root = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(root.path(), || {
            let project = root.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let project = project.canonicalize().unwrap();
            let auto = uuid::Uuid::new_v4().to_string();
            let default = uuid::Uuid::new_v4().to_string();
            let sources = [
                source_conversation(
                    root.path(),
                    &project,
                    &auto,
                    &[("never", "workspace-write")],
                    ("never", "workspace-write"),
                ),
                source_conversation(
                    root.path(),
                    &project,
                    &default,
                    &[("on-request", "workspace-write")],
                    ("on-request", "workspace-write"),
                ),
            ];
            let roots = policy::CanonicalRoots::from_untrusted([project.clone()]);
            let catalog = crate::rc::runtime_catalog::Catalog::open().unwrap();
            for _ in &sources {
                catalog.reconcile(&roots).unwrap();
            }
            let daemon = super::super::tests::rpc_test_daemon(HashMap::new(), Roster::default());
            let mut state = daemon.try_lock().unwrap();
            state.opts.local_owner = true;
            state.mirror.bind("ws", "project", &project).unwrap();
            let claim = |role: &str| crate::protocol::CallerClaim {
                account_id: Some("member".into()),
                username: None,
                role: role.into(),
                workspace_id: "ws".into(),
            };
            let reason = |source: &crate::rc::runtime_sources::RuntimeSource,
                          native: &str,
                          role: &str| {
                let mut frame = Frame::request(
                    method::SESSION_ENQUEUE,
                    serde_json::json!({"workspace_id":"ws","session_id":source.session_ref(native),
                        "client_msg_id":uuid::Uuid::new_v4().to_string(),"message":"hello"}),
                );
                frame.caller = Some(claim(role));
                let watch = super::super::source_watch::SourceWatch::resolve(
                    &source.session_ref(native),
                    &roots,
                )
                .unwrap()
                .expect("the source conversation resolves");
                state
                    .prepare_inbox_target(&frame, None, Some(watch))
                    .err()
                    .and_then(|error| error.data)
                    .and_then(|data| data["reason"].as_str().map(str::to_owned))
            };
            assert_eq!(
                reason(&sources[0], &auto, "operator").as_deref(),
                Some("native_mode_changed")
            );
            assert_ne!(
                reason(&sources[0], &auto, "owner").as_deref(),
                Some("native_mode_changed")
            );
            assert_ne!(
                reason(&sources[1], &default, "operator").as_deref(),
                Some("native_mode_changed")
            );
        });
    }

    /// Pins that a non-owner is judged only by the mode the Codex index records now, at all four
    /// places a non-owner reaches a source thread: a turn in a running session, a native message,
    /// its delivery, and a resume. The rollout's history says Default throughout. With the index
    /// locked by another connection (after the thread was set to Auto), and with the index gone,
    /// the operator is refused at each place (`native_mode_changed`, `native_mode_owner_only` on
    /// resume) and the owner is not. A delivery prepared while the thread was in Default is refused
    /// for the operator once it moves to Auto. A gate that took the history's mode when the index
    /// could not be read, or a delivery that refused only Bypass, let the operator through.
    #[tokio::test]
    async fn a_non_owner_is_judged_only_by_the_current_index_mode() {
        let root = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(root.path(), || {
            let project = root.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let project = project.canonicalize().unwrap();
            let roots = policy::CanonicalRoots::from_untrusted([project.clone()]);
            let conversation = |native: &str| {
                source_conversation(
                    root.path(),
                    &project,
                    native,
                    &[("on-request", "workspace-write")],
                    ("on-request", "workspace-write"),
                )
            };
            let (locked, absent, moved) = (
                uuid::Uuid::new_v4().to_string(),
                uuid::Uuid::new_v4().to_string(),
                uuid::Uuid::new_v4().to_string(),
            );
            let sources = [
                conversation(&locked),
                conversation(&absent),
                conversation(&moved),
            ];
            let catalog = crate::rc::runtime_catalog::Catalog::open().unwrap();
            for _ in &sources {
                catalog.reconcile(&roots).unwrap();
            }
            let index = |native: &str| root.path().join(format!("home-{native}/state_1.sqlite"));
            let to_auto = |native: &str| {
                rusqlite::Connection::open(index(native))
                    .unwrap()
                    .execute(
                        "UPDATE threads SET approval_mode='never', sandbox_policy='{\"type\":\"workspace-write\"}'",
                        [],
                    )
                    .unwrap();
            };
            let mut sessions = HashMap::new();
            for (source, native) in sources.iter().zip([&locked, &absent, &moved]) {
                let (tx, _rx) = mpsc::channel(1);
                let mut live = super::super::tests::rpc_test_live(
                    native,
                    1,
                    tx,
                    crate::protocol::PermissionMode::Default,
                );
                live.info.runtime = "codex".into();
                live.info.workspace_id = "ws".into();
                live.info.native_source = Some(NativeSourceRef {
                    source_id: source.source_id.clone(),
                    generation: source.generation,
                });
                live.runtime_thread_id = Some(native.clone());
                live.admitted_mode = Some(crate::protocol::PermissionMode::Default);
                sessions.insert(native.clone(), live);
            }
            let daemon = super::super::tests::rpc_test_daemon(sessions, Roster::default());
            let mut state = daemon.try_lock().unwrap();
            state.opts.local_owner = true;
            state.mirror.bind("ws", "project", &project).unwrap();
            let (frames, _) = mpsc::channel(16);
            let claim = |role: &str| crate::protocol::CallerClaim {
                account_id: Some("member".into()),
                username: None,
                role: role.into(),
                workspace_id: "ws".into(),
            };
            let reason = |error: RpcError| {
                error
                    .data
                    .and_then(|data| data["reason"].as_str().map(str::to_owned))
            };
            let message = |state: &Daemon,
                           source: &crate::rc::runtime_sources::RuntimeSource,
                           native: &str,
                           role: &str| {
                let mut frame = Frame::request(
                    method::SESSION_ENQUEUE,
                    serde_json::json!({"workspace_id":"ws","session_id":source.session_ref(native),
                        "client_msg_id":uuid::Uuid::new_v4().to_string(),"message":"hello"}),
                );
                frame.caller = Some(claim(role));
                let watch = super::super::source_watch::SourceWatch::resolve(
                    &source.session_ref(native),
                    &roots,
                )
                .unwrap()
                .expect("the source conversation resolves");
                state.prepare_inbox_target(&frame, None, Some(watch))
            };
            let delivered = |prepared: &crate::rc::native_inbox::Prepared| {
                prepared
                    .validate_source()
                    .err()
                    .and_then(|error| error.downcast::<RpcError>().ok().and_then(reason))
            };
            let resume = |state: &mut Daemon,
                          source: &crate::rc::runtime_sources::RuntimeSource,
                          native: &str,
                          role: &str| {
                state.sessions.remove(native);
                let result = state.prepare_source_resume(
                    SessionResume {
                        project_id: None,
                        workspace_id: "ws".into(),
                        session_id: source.session_ref(native),
                        prompt: None,
                        by: None,
                        agent: None,
                        expected_agent_id: None,
                        branch: None,
                        attach_only: false,
                    },
                    &claim(role),
                    &frames,
                    (&source.source_id, native),
                    None,
                    &Default::default(),
                );
                state.opening_sessions.clear();
                result.err().map(reason)
            };
            let changed = Some("native_mode_changed".to_owned());

            // Deliveries prepared while every thread is in Default.
            let prepared: Vec<_> = sources
                .iter()
                .zip([&locked, &absent, &moved])
                .map(|(source, native)| {
                    (
                        message(&state, source, native, "operator").unwrap(),
                        message(&state, source, native, "owner").unwrap(),
                    )
                })
                .collect();

            to_auto(&locked);
            let lock = rusqlite::Connection::open(index(&locked)).unwrap();
            lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
            std::fs::remove_file(index(&absent)).unwrap();
            to_auto(&moved);

            for (at, (source, native)) in sources.iter().zip([&locked, &absent]).enumerate() {
                let drive = |role: &str| {
                    state
                        .session_channel(native, &claim(role), Need::Drive)
                        .err()
                        .map(reason)
                };
                assert_eq!(drive("operator"), Some(changed.clone()), "{at}");
                assert_eq!(drive("owner"), None, "{at}");
                assert_eq!(
                    message(&state, source, native, "operator")
                        .err()
                        .map(reason),
                    Some(changed.clone()),
                    "{at}"
                );
                assert!(message(&state, source, native, "owner").is_ok(), "{at}");
                assert_eq!(delivered(&prepared[at].0), changed, "{at}");
                assert_ne!(delivered(&prepared[at].1), changed, "{at}");
                assert_eq!(
                    resume(&mut state, source, native, "operator"),
                    Some(Some("native_mode_owner_only".to_owned())),
                    "{at}"
                );
                assert_eq!(resume(&mut state, source, native, "owner"), None, "{at}");
            }
            // The index readable and the thread now in Auto: the delivery prepared in Default is
            // refused for the operator.
            assert_eq!(delivered(&prepared[2].0), changed);
            assert_ne!(delivered(&prepared[2].1), changed);
            lock.execute_batch("ROLLBACK").unwrap();
        });
    }
}
