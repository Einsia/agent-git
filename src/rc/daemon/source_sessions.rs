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
        let thread = context.locate(native, &roots).map_err(unavailable)?;
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
        let settings = context.settings(&thread).map_err(unavailable)?;
        let danger = danger::authorize_source(
            &self.roster,
            caller,
            source_id,
            native,
            &thread.cwd.to_string_lossy(),
            settings.permission_mode,
        )?;
        let logical = if prior.is_some() {
            p.session_id.clone()
        } else {
            self.roster
                .logical_for_thread_in("codex", Some(source_id), native, &p.workspace_id)
                .unwrap_or_else(crate::domain::meta::mint_session_id)
        };
        if self.ending_in(&logical, caller) {
            return Err(RpcError::new(
                ErrorCode::SessionBusy,
                "this conversation is still settling",
            ));
        }
        if let Some(info) = self.supervised_in(&logical, caller) {
            return Ok(SessionOpening::Ready(
                serde_json::to_value(SessionResumeResult {
                    session: self.stamped(info),
                })
                .unwrap(),
            ));
        }
        let prior = prior.or_else(|| self.roster.get(&logical).cloned());
        if prior
            .as_ref()
            .is_some_and(|entry| !entry.guard_attempts.is_empty())
            && settings.permission_mode != Some(crate::protocol::PermissionMode::Plan)
        {
            return Err(RpcError::new(
                ErrorCode::SessionBusy,
                "native permissions are unresolved; select Plan in the native client before reconnecting",
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
            gist: None,
            title: None,
            dangerous: false,
            permission_mode: settings.permission_mode,
            created_at: now.clone(),
            updated_at: now,
        };
        let spec = LaunchSpec {
            cwd: thread.cwd,
            resume_from: Some(native.into()),
            agit_session: lineage,
            model: None,
            dangerous: false,
            permission_mode: settings.permission_mode,
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
                        "{}\n",
                        serde_json::json!({
                            "type":"session_meta", "payload":{"id":native,"cwd":project}
                        })
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
            };
            let mut ids = vec![];
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
                ids.push(spawn.info.session_id);
            }
            assert_ne!(ids[0], ids[1]);
            assert_eq!(state.opening_sessions.len(), 2);
            state.opening_sessions.clear();
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
}
