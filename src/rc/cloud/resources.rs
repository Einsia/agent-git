//! Resource coordinates come from executor records and executor-produced catalogs.

use agit_peer::access::{Access, Policy, Principal, Resource};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

pub type SourceIdentity = crate::protocol::NativeSourceRef;

#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub native_id: String,
    pub source: Option<SourceIdentity>,
    pub runtime: String,
    pub cwd: String,
    pub project: Option<String>,
}

impl Session {
    pub fn access(&self, policy: &Policy, principal: &Principal) -> Access {
        let canonical = self
            .source
            .as_ref()
            .map(|source| source.session_ref(&self.native_id));
        let mut aliases = vec![&self.id];
        if let Some(canonical) = &canonical {
            aliases.push(canonical);
        }
        if self.source.is_none() {
            aliases.push(&self.native_id);
        }
        let explicit = policy
            .rules()
            .iter()
            .filter(|rule| &rule.principal == principal)
            .filter(|rule| matches!(&rule.resource, Resource::Session(id) if aliases.contains(&id)))
            .collect::<Vec<_>>();
        if explicit.iter().any(|rule| rule.access == Access::Deny) {
            return Access::Deny;
        }
        for alias in aliases {
            if let Some(rule) = explicit
                .iter()
                .find(|rule| rule.resource == Resource::Session(alias.clone()))
            {
                return rule.access;
            }
        }
        policy.access(principal, Some(&self.id), self.project.as_deref())
    }
}

#[derive(Default, Clone)]
pub struct Resources {
    pub projects: HashMap<String, PathBuf>,
    sessions: HashMap<String, Session>,
    catalog: HashMap<String, Session>,
    catalog_order: std::collections::VecDeque<String>,
    controller_sessions: HashMap<String, ControllerSession>,
    available_sources: HashMap<String, u64>,
}

#[derive(Clone)]
struct ControllerSession {
    session: Session,
    lifetime: std::sync::Weak<()>,
}

const CATALOG_CACHE_LIMIT: usize = 2048;

impl Resources {
    pub fn refresh(&mut self, current: Self) {
        self.projects = current.projects;
        self.available_sources = current.available_sources;
        self.controller_sessions
            .retain(|_, pinned| pinned.lifetime.strong_count() > 0);
        for pinned in self.controller_sessions.values_mut() {
            pinned.session.project = self
                .projects
                .iter()
                .filter(|(_, root)| Path::new(&pinned.session.cwd).starts_with(root))
                .max_by_key(|(_, root)| root.components().count())
                .map(|(id, _)| id.clone());
        }
        let projects = self.projects.clone();
        for session in self.sessions.values_mut().chain(self.catalog.values_mut()) {
            if let Some(known) = current.sessions.get(&session.id).or_else(|| {
                session
                    .source
                    .is_none()
                    .then(|| current.sessions.get(&session.native_id))
                    .flatten()
            }) {
                *session = known.clone();
            }
            session.project = projects
                .iter()
                .filter(|(_, root)| Path::new(&session.cwd).starts_with(root))
                .max_by_key(|(_, root)| root.components().count())
                .map(|(id, _)| id.clone());
        }
        self.sessions.extend(current.sessions);
    }

    pub fn load() -> crate::Result<Self> {
        let mut resources = Self::default();
        if let Ok(registry) = super::super::runtime_sources::Registry::open()
            && let Ok(sources) = registry.list()
        {
            for source in sources
                .into_iter()
                .filter(|source| source.validate().is_ok())
            {
                resources
                    .available_sources
                    .insert(source.source_id, source.generation);
            }
        }
        let mirror = super::super::mirror::Mirror::load();
        for workspace in mirror.to_local() {
            if workspace.workspace_id != super::super::endpoint::WORKSPACE {
                continue;
            }
            for project in workspace.projects {
                if let Some(path) =
                    mirror.project_path(&workspace.workspace_id, &project.project_id)
                {
                    resources.projects.insert(project.project_id, path);
                }
            }
        }
        for (id, entry) in super::super::roster::Roster::try_load()?.sessions {
            if entry.workspace_id != super::super::endpoint::WORKSPACE {
                continue;
            }
            resources.insert_managed(Session {
                id,
                native_id: entry.thread_id,
                source: entry.native_source,
                runtime: entry.runtime,
                cwd: entry.cwd,
                project: entry.project_id,
            });
        }
        Ok(resources)
    }

    fn insert_managed(&mut self, session: Session) {
        if session.source.is_none() {
            self.insert(session);
        } else {
            // Managed identities remain addressable independently of catalog page eviction.
            self.sessions.insert(session.id.clone(), session);
        }
    }

    fn insert(&mut self, session: Session) {
        if session.source.is_some() {
            if !self.catalog.contains_key(&session.id) {
                if self.catalog.len() >= CATALOG_CACHE_LIMIT
                    && let Some(oldest) = self.catalog_order.pop_front()
                {
                    self.catalog.remove(&oldest);
                }
                self.catalog_order.push_back(session.id.clone());
            }
            self.catalog.insert(session.id.clone(), session);
            return;
        }
        if self.sessions.len() >= 8192 {
            return;
        }
        if session.source.is_none() && !session.native_id.is_empty() {
            self.sessions
                .insert(session.native_id.clone(), session.clone());
        }
        self.sessions.insert(session.id.clone(), session);
    }

    pub fn resumed(&mut self, source: &Session, row: &Value) {
        if row["workspace_id"].as_str() != Some(super::super::endpoint::WORKSPACE) {
            return;
        }
        let Some(id) = row["session_id"].as_str() else {
            return;
        };
        let mut session = source.clone();
        session.id = id.into();
        // Every alias must acquire the logical identity before a response exposes it.
        for known in self.sessions.values_mut() {
            if known.id == source.id
                || (known.source == source.source
                    && !source.native_id.is_empty()
                    && known.native_id == source.native_id)
            {
                *known = session.clone();
            }
        }
        self.insert_managed(session);
    }

    pub fn session(&self, id: &str) -> Option<&Session> {
        self.controller_sessions
            .get(id)
            .filter(|pinned| {
                pinned.lifetime.strong_count() > 0
                    && pinned.session.source.as_ref().is_some_and(|source| {
                        self.available_sources.get(&source.source_id) == Some(&source.generation)
                    })
                    && pinned.session.project.is_some()
            })
            .map(|pinned| &pinned.session)
            .or_else(|| self.sessions.get(id))
            .or_else(|| self.catalog.get(id))
    }

    pub(super) fn controller_session(&self, id: &str) -> Option<&Session> {
        if !id.starts_with("local-") {
            return self.session(id);
        }
        let pinned = self.controller_sessions.get(id)?;
        (pinned.lifetime.strong_count() > 0
            && pinned.session.project.is_some()
            && pinned.session.source.as_ref().is_some_and(|source| {
                self.available_sources.get(&source.source_id) == Some(&source.generation)
            }))
        .then_some(&pinned.session)
    }

    pub(super) fn prepare_controller_source(&mut self, id: &str) -> crate::Result<Session> {
        let session = self
            .sessions
            .values()
            .find(|session| {
                session.source.as_ref().is_some_and(|source| {
                    source.session_ref(&session.native_id) == id
                        && self.available_sources.get(&source.source_id) == Some(&source.generation)
                })
            })
            .cloned();
        let session = match session {
            Some(session) => Some(session),
            None => self.resolve_catalog(id)?,
        };
        let mut session = session
            .ok_or_else(|| anyhow::anyhow!("controller source conversation is unavailable"))?;
        let source = session
            .source
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("controller source identity is missing"))?;
        let context = super::super::runtime_context::RuntimeContext::resolve(
            &super::super::runtime_sources::Registry::open()?,
            &source.source_id,
        )?;
        anyhow::ensure!(
            context.source.generation == source.generation
                && source.session_ref(&session.native_id) == id,
            "controller source identity changed"
        );
        let roots =
            super::super::policy::CanonicalRoots::from_untrusted(self.projects.values().cloned());
        let native = context.locate(&session.native_id, &roots)?;
        anyhow::ensure!(
            native.cwd == Path::new(&session.cwd),
            "controller conversation directory changed"
        );
        super::super::local_goal::validate_header(
            &native.transcript,
            &session.native_id,
            &native.cwd,
        )?;
        session.id = id.into();
        Ok(session)
    }

    pub(super) fn pin_controller_source(
        &mut self,
        session: Session,
    ) -> crate::Result<std::sync::Arc<()>> {
        self.controller_sessions
            .retain(|_, pinned| pinned.lifetime.strong_count() > 0);
        anyhow::ensure!(
            self.controller_sessions.len() < 2048
                || self.controller_sessions.contains_key(&session.id),
            "source controller capacity exceeded"
        );
        let lifetime = self
            .controller_sessions
            .get(&session.id)
            .and_then(|pinned| pinned.lifetime.upgrade())
            .unwrap_or_else(|| std::sync::Arc::new(()));
        self.controller_sessions.insert(
            session.id.clone(),
            ControllerSession {
                session,
                lifetime: std::sync::Arc::downgrade(&lifetime),
            },
        );
        Ok(lifetime)
    }

    pub fn resolve_catalog(&mut self, id: &str) -> crate::Result<Option<Session>> {
        self.resolve_catalog_with(id, |id| {
            super::super::runtime_catalog::Catalog::open()?.lookup(id)
        })
    }

    fn resolve_catalog_with(
        &mut self,
        id: &str,
        lookup: impl FnOnce(&str) -> crate::Result<Option<super::super::runtime_context::CatalogRow>>,
    ) -> crate::Result<Option<Session>> {
        if let Some(session) = self.session(id) {
            return Ok(Some(session.clone()));
        }
        if id.len() != 70
            || !id.starts_with("local-")
            || !id[6..].bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Ok(None);
        }
        let Some(row) = lookup(id)? else {
            return Ok(None);
        };
        let session = self.catalog_session(&serde_json::to_value(row)?);
        if let Some(session) = &session {
            self.insert(session.clone());
        }
        Ok(session)
    }

    fn catalog_session(&self, row: &Value) -> Option<Session> {
        let cwd = row["cwd"].as_str()?;
        Some(Session {
            id: row["session_ref"].as_str()?.into(),
            native_id: row["native_session_id"].as_str()?.into(),
            runtime: row["runtime"].as_str()?.into(),
            cwd: cwd.into(),
            source: Some(SourceIdentity {
                source_id: row["source_id"].as_str()?.into(),
                generation: row["source_generation"].as_u64()?,
            }),
            project: self.project_for_path(Path::new(cwd)),
        })
    }

    pub fn project_for_path(&self, path: &Path) -> Option<String> {
        self.projects
            .iter()
            .filter(|(_, root)| path.starts_with(root))
            .max_by_key(|(_, root)| root.components().count())
            .map(|(id, _)| id.clone())
    }

    pub fn observe(&mut self, method: &str, result: &Value) {
        if method == "project.bind" {
            self.observe_project(result);
        }
        if method == "workspace.list" {
            self.projects.clear();
            for workspace in result["workspaces"].as_array().into_iter().flatten() {
                if workspace["workspace_id"].as_str() == Some(super::super::endpoint::WORKSPACE) {
                    for project in workspace["projects"].as_array().into_iter().flatten() {
                        self.observe_project(project);
                    }
                }
            }
        }
        if method == "session.catalog.list" {
            for row in result["rows"].as_array().into_iter().flatten() {
                if let Some(session) = self.catalog_session(row) {
                    self.insert(session);
                }
            }
        }
        if method == "session.list" {
            for row in result["sessions"].as_array().into_iter().flatten() {
                self.observe_session(row);
            }
            for row in result["local"].as_array().into_iter().flatten() {
                let (Some(id), Some(runtime), Some(cwd)) = (
                    row["runtime_session_id"].as_str(),
                    row["runtime"].as_str(),
                    row["cwd"].as_str(),
                ) else {
                    continue;
                };
                if self.sessions.contains_key(id) {
                    continue;
                }
                let project = self.project_for_path(Path::new(cwd));
                self.insert(Session {
                    id: id.into(),
                    native_id: id.into(),
                    source: None,
                    runtime: runtime.into(),
                    cwd: cwd.into(),
                    project,
                });
            }
        }
        if matches!(
            method,
            "session.start" | "session.resume" | "session.watch" | "session.subscribe"
        ) {
            self.observe_session(&result["session"]);
        }
    }

    fn observe_project(&mut self, row: &Value) {
        let (Some(id), Some(path)) = (row["project_id"].as_str(), row["local_path"].as_str())
        else {
            return;
        };
        if !id.is_empty() && Path::new(path).is_absolute() && self.projects.len() < 8192 {
            self.projects.insert(id.into(), path.into());
        }
    }

    fn observe_session(&mut self, row: &Value) {
        let Some(id) = row["session_id"].as_str() else {
            return;
        };
        if row["workspace_id"].as_str() != Some(super::super::endpoint::WORKSPACE) {
            return;
        }
        if let Some(source) = row.get("native_source").filter(|source| !source.is_null()) {
            let Ok(source) = serde_json::from_value::<SourceIdentity>(source.clone()) else {
                return;
            };
            let project = row["project_id"].as_str().map(str::to_owned);
            let cwd = project
                .as_ref()
                .and_then(|id| self.projects.get(id))
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_default();
            self.insert_managed(Session {
                id: id.into(),
                native_id: row["runtime_session_id"]
                    .as_str()
                    .unwrap_or_default()
                    .into(),
                source: Some(source),
                runtime: row["runtime"].as_str().unwrap_or_default().into(),
                cwd,
                project,
            });
            return;
        }
        if let Some(mut session) = self.sessions.get(id).cloned() {
            if let Some(native_id) = row["runtime_session_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                && row["runtime"].as_str() == Some(session.runtime.as_str())
                && session.native_id != native_id
            {
                session.native_id = native_id.into();
                // Watch and native aliases retain the same logical permission boundary.
                for known in self
                    .sessions
                    .values_mut()
                    .filter(|known| known.id == session.id)
                {
                    *known = session.clone();
                }
                self.insert(session);
            }
            return;
        }
        let project = row["project_id"].as_str().map(str::to_owned);
        let cwd = project
            .as_ref()
            .and_then(|id| self.projects.get(id))
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default();
        self.insert(Session {
            id: id.into(),
            native_id: row["runtime_session_id"]
                .as_str()
                .unwrap_or_default()
                .into(),
            source: None,
            runtime: row["runtime"].as_str().unwrap_or_default().into(),
            cwd,
            project,
        });
    }

    pub fn watch_alias(&mut self, id: &str) {
        if let Some(session) = self.session(id).cloned() {
            let stream =
                super::super::daemon::watch_stream_id(super::super::endpoint::WORKSPACE, id);
            if self.sessions.len() < 8192 {
                self.sessions.insert(stream, session);
            }
        }
    }

    pub fn filter(&self, method: &str, result: &mut Value, policy: &Policy, principal: &Principal) {
        if method == "session.catalog.list" {
            // Scan health survives permission filtering even when a delta has no visible sources.
            let coverage_status = match result["coverage"].as_array() {
                Some(sources) if !sources.is_empty() => {
                    if sources.iter().any(|source| {
                        !matches!(source["status"].as_str(), Some("ready" | "scanning"))
                    }) {
                        "unavailable"
                    } else if sources.iter().any(|source| source["status"] == "scanning") {
                        "scanning"
                    } else {
                        "ready"
                    }
                }
                _ if result["complete"] == true => "ready",
                _ => "unavailable",
            };
            result["coverage_status"] = serde_json::json!(coverage_status);
            if let Some(rows) = result["rows"].as_array_mut() {
                rows.retain(|row| {
                    self.catalog_session(row)
                        .is_some_and(|session| session.access(policy, principal).can_read())
                });
            }
            if !policy.access(principal, None, None).is_admin() {
                // Device-wide source health must not reveal unrelated profiles to a project reader.
                let visible = result["rows"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|row| row["source_id"].as_str().map(str::to_owned))
                    .collect::<std::collections::HashSet<_>>();
                if let Some(coverage) = result["coverage"].as_array_mut() {
                    coverage.retain(|source| {
                        source["source_id"]
                            .as_str()
                            .is_some_and(|id| visible.contains(id))
                    });
                }
            }
        }
        if method == "session.list" {
            for field in ["sessions", "local"] {
                if let Some(rows) = result[field].as_array_mut() {
                    rows.retain(|row| {
                        let id = row[if field == "local" {
                            "runtime_session_id"
                        } else {
                            "session_id"
                        }]
                        .as_str();
                        id.and_then(|id| self.session(id))
                            .is_some_and(|session| session.access(policy, principal).can_read())
                    });
                }
            }
        }
        if method == "workspace.list"
            && let Some(workspaces) = result["workspaces"].as_array_mut()
        {
            for workspace in workspaces.iter_mut() {
                if let Some(projects) = workspace["projects"].as_array_mut() {
                    projects.retain(|project| {
                        project["project_id"].as_str().is_some_and(|id| {
                            policy.access(principal, None, Some(id)).can_read()
                                || self.sessions.values().chain(self.catalog.values()).any(
                                    |session| {
                                        session.project.as_deref() == Some(id)
                                            && session.access(policy, principal).can_read()
                                    },
                                )
                        })
                    });
                }
            }
            workspaces.retain(|workspace| {
                workspace["projects"]
                    .as_array()
                    .is_some_and(|projects| !projects.is_empty())
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agit_peer::access::Rule;
    use serde_json::json;

    #[test]
    fn catalogs_and_native_aliases_preserve_session_denials_over_project_access() {
        let principal = Principal {
            issuer: "https://cloud.example".into(),
            account_id: "alice".into(),
        };
        let policy = Policy::new(
            1,
            vec![
                Rule {
                    principal: principal.clone(),
                    resource: Resource::Project("project".into()),
                    access: Access::Read,
                },
                Rule {
                    principal: principal.clone(),
                    resource: Resource::Session("hidden-native".into()),
                    access: Access::Deny,
                },
            ],
        )
        .unwrap();
        let mut resources = Resources::default();
        resources
            .projects
            .insert("project".into(), PathBuf::from("/workspace/project"));
        resources.observe("session.start", &json!({"session": {
            "session_id":"hidden-logical", "workspace_id":"local-owner", "project_id":"project", "runtime":"codex"
        }}));
        assert!(
            resources
                .session("hidden-logical")
                .unwrap()
                .native_id
                .is_empty()
        );
        let mut result = json!({"sessions":[{"session_id":"hidden-logical","runtime_session_id":"hidden-native","workspace_id":"local-owner","project_id":"project","runtime":"codex"}],
            "local":[{"runtime_session_id":"visible","runtime":"codex","cwd":"/workspace/project"}, {"runtime_session_id":"foreign","runtime":"codex","cwd":"/private/other"}]});
        resources.observe("session.list", &result);
        resources.filter("session.list", &mut result, &policy, &principal);
        assert!(result["sessions"].as_array().unwrap().is_empty());
        assert_eq!(result["local"].as_array().unwrap().len(), 1);
        assert_eq!(result["local"][0]["runtime_session_id"], "visible");
        resources.watch_alias("hidden-native");
        assert_eq!(
            resources
                .session("agit-watch-local-owner-hidden-native")
                .unwrap()
                .access(&policy, &principal),
            Access::Deny
        );
    }
    #[test]
    fn source_watch_alias_retains_read_authority_without_granting_writes() {
        let principal = Principal {
            issuer: "https://cloud.example".into(),
            account_id: "viewer".into(),
        };
        let source = SourceIdentity {
            source_id: "source-a".into(),
            generation: 1,
        };
        let reference = source.session_ref("native");
        let policy = Policy::new(
            1,
            vec![Rule {
                principal: principal.clone(),
                resource: Resource::Session(reference.clone()),
                access: Access::Read,
            }],
        )
        .unwrap();
        let mut resources = Resources::default();
        resources.insert(Session {
            id: reference.clone(),
            native_id: "native".into(),
            source: Some(source),
            runtime: "codex".into(),
            cwd: "/project".into(),
            project: None,
        });
        let watch =
            crate::protocol::Frame::request("session.watch", json!({"session_id":reference}));
        super::super::access::authorize(watch, &principal, &policy, &mut resources).unwrap();
        let stream = super::super::super::daemon::watch_stream_id(
            super::super::super::endpoint::WORKSPACE,
            &reference,
        );
        for (method, id, allowed) in [
            ("session.subscribe", &stream, true),
            ("session.unwatch", &reference, true),
            ("session.history", &reference, true),
            ("session.goal.read", &reference, true),
            ("session.resume", &reference, false),
            ("session.enqueue", &stream, false),
        ] {
            let frame = crate::protocol::Frame::request(method, json!({"session_id":id}));
            assert_eq!(
                super::super::access::authorize(frame, &principal, &policy, &mut resources).is_ok(),
                allowed
            );
        }
    }

    #[test]
    fn source_permission_survives_logical_attachment_without_granting_copied_ids() {
        let principal = Principal {
            issuer: "https://cloud.example".into(),
            account_id: "member".into(),
        };
        let source = SourceIdentity {
            source_id: "source-a".into(),
            generation: 1,
        };
        let reference = source.session_ref("copied");
        let policy = Policy::new(
            1,
            vec![Rule {
                principal: principal.clone(),
                resource: Resource::Session(reference.clone()),
                access: Access::Control,
            }],
        )
        .unwrap();
        let mut resources = Resources::default();
        let session = Session {
            id: reference,
            native_id: "copied".into(),
            source: Some(source),
            runtime: "codex".into(),
            cwd: "/project".into(),
            project: None,
        };
        resources.insert(session.clone());
        let queue = crate::protocol::Frame::request(
            "session.enqueue",
            json!({"session_id":session.id,"source_id":"forged", "native_session_id":"forged",
                "client_msg_id":uuid::Uuid::new_v4().to_string(),"message":"Queue in the existing native conversation"}),
        );
        let (admitted, _) =
            super::super::access::authorize(queue.clone(), &principal, &policy, &mut resources)
                .unwrap();
        assert_eq!(admitted.params.as_ref().unwrap()["source_id"], "source-a");
        assert_eq!(
            admitted.params.as_ref().unwrap()["native_session_id"],
            "copied"
        );
        let read_only = Policy::new(
            2,
            vec![Rule {
                principal: principal.clone(),
                resource: Resource::Session(session.id.clone()),
                access: Access::Read,
            }],
        )
        .unwrap();
        assert!(
            super::super::access::authorize(queue, &principal, &read_only, &mut resources).is_err()
        );
        let request =
            crate::protocol::Frame::request("session.resume", json!({"session_id":session.id}));
        super::super::access::authorize(request, &principal, &policy, &mut resources).unwrap();
        resources.resumed(&session, &json!({"session_id":"agit-managed", "workspace_id":super::super::super::endpoint::WORKSPACE}));
        let managed = resources.session("agit-managed").unwrap();
        assert_eq!(managed.access(&policy, &principal), Access::Control);
        let mut copied = managed.clone();
        for method in [
            "session.enqueue",
            "session.setModel",
            "session.setPermissionMode",
            "approval.decide",
            "turn.interrupt",
        ] {
            let request = crate::protocol::Frame::request(
                method,
                json!({"session_id":"agit-managed",
                "source_id":"forged","native_session_id":"forged","client_msg_id":uuid::Uuid::new_v4().to_string(),
                "message":"queue message","effort":"high","mode":"plan","approval_id":"request","decision":"deny","scope":"once"}),
            );
            let (admitted, _) = super::super::access::authorize(
                request.clone(),
                &principal,
                &policy,
                &mut resources,
            )
            .unwrap();
            assert_eq!(admitted.params.as_ref().unwrap()["source_id"], "source-a");
            assert_eq!(
                admitted.params.as_ref().unwrap()["native_session_id"],
                "copied"
            );
            let read_only = Policy::new(
                2,
                vec![Rule {
                    principal: principal.clone(),
                    resource: Resource::Session("agit-managed".into()),
                    access: Access::Read,
                }],
            )
            .unwrap();
            assert!(
                super::super::access::authorize(request, &principal, &read_only, &mut resources)
                    .is_err()
            );
        }
        copied.id = "agit-other".into();
        copied.source.as_mut().unwrap().source_id = "source-b".into();
        assert_eq!(copied.access(&policy, &principal), Access::Deny);
    }

    #[test]
    fn managed_source_identity_survives_catalog_eviction_without_native_aliases() {
        let mut resources = Resources::default();
        resources.observe(
            "session.start",
            &json!({"session": {
                "session_id":"managed-alpha", "runtime_session_id":"copied-native",
                "workspace_id":"local-owner", "runtime":"codex",
                "native_source":{"source_id":"alpha", "generation":1}
            }}),
        );
        for index in 0..=CATALOG_CACHE_LIMIT {
            resources.insert(Session {
                id: format!("catalog-{index}"),
                native_id: "copied-native".into(),
                source: Some(SourceIdentity {
                    source_id: "beta".into(),
                    generation: 1,
                }),
                runtime: "codex".into(),
                cwd: String::new(),
                project: None,
            });
        }
        assert_eq!(
            resources
                .session("managed-alpha")
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                .source_id,
            "alpha"
        );
        assert!(resources.session("copied-native").is_none());
        assert!(resources.session("catalog-0").is_none());
        assert_eq!(resources.catalog.len(), CATALOG_CACHE_LIMIT);
        resources.observe(
            "session.list",
            &json!({"sessions":[{
                "session_id":"managed-alpha", "runtime_session_id":"rotated-native",
                "workspace_id":"local-owner", "runtime":"codex",
                "native_source":{"source_id":"alpha", "generation":2}
            }]}),
        );
        let managed = resources.session("managed-alpha").unwrap();
        assert_eq!(managed.native_id, "rotated-native");
        assert_eq!(managed.source.as_ref().unwrap().generation, 2);
        assert!(resources.session("rotated-native").is_none());
    }

    #[test]
    fn source_catalog_permissions_never_alias_copied_native_ids() {
        let principal = Principal {
            issuer: "https://cloud.example".into(),
            account_id: "reader".into(),
        };
        let policy = Policy::new(
            1,
            vec![
                Rule {
                    principal: principal.clone(),
                    resource: Resource::Project("allowed".into()),
                    access: Access::Read,
                },
                Rule {
                    principal: principal.clone(),
                    resource: Resource::Session("copied".into()),
                    access: Access::Admin,
                },
            ],
        )
        .unwrap();
        let mut resources = Resources::default();
        resources
            .projects
            .insert("allowed".into(), "/workspace/allowed".into());
        resources
            .projects
            .insert("private".into(), "/workspace/private".into());
        let mut result = json!({"rows":[
            {"session_ref":"local-a","source_id":"source-a","source_generation":1,"native_session_id":"copied","runtime":"codex","cwd":"/workspace/allowed"},
            {"session_ref":"local-b","source_id":"source-b","source_generation":2,"native_session_id":"copied","runtime":"codex","cwd":"/workspace/private"}
        ],"complete":false,"coverage":[{"source_id":"source-a","status":"ready"},{"source_id":"source-b","status":"scanning"}]});
        resources.observe("session.catalog.list", &result);
        assert!(resources.session("copied").is_none());
        resources.filter("session.catalog.list", &mut result, &policy, &principal);
        assert_eq!(result["rows"].as_array().unwrap().len(), 1);
        assert_eq!(result["rows"][0]["session_ref"], "local-a");
        assert_eq!(
            result["coverage"],
            json!([{"source_id":"source-a","status":"ready"}])
        );
        assert_eq!(result["coverage_status"], "scanning");
        for status in ["scanning", "unavailable", "ready"] {
            let mut delta = json!({"rows":[],"complete":status == "ready",
                "coverage":[{"source_id":"source-a","status":status}]});
            resources.filter("session.catalog.list", &mut delta, &policy, &principal);
            assert_eq!(delta["coverage_status"], status);
            assert_eq!(delta["coverage"], json!([]));
            assert_eq!(delta["rows"], json!([]));
        }
        let frame = crate::protocol::Frame::request(
            "session.catalog.settings",
            json!({
                "session_id":"local-a","source_id":"source-b","source_generation":2,"native_session_id":"foreign","expected_cwd":"/workspace/private"
            }),
        );
        let (authorized, _) =
            super::super::access::authorize(frame, &principal, &policy, &mut resources).unwrap();
        let params = authorized.params.unwrap();
        assert_eq!(params["source_id"], "source-a");
        assert_eq!(params["native_session_id"], "copied");
        assert_eq!(params["expected_cwd"], "/workspace/allowed");
        assert_eq!(authorized.caller.unwrap().role, "viewer");
        let denied = crate::protocol::Frame::request(
            "session.catalog.settings",
            json!({"session_id":"local-b"}),
        );
        assert!(
            super::super::access::authorize(denied, &principal, &policy, &mut resources).is_err()
        );
        let denied =
            crate::protocol::Frame::request("session.resume", json!({"session_id":"local-a"}));
        assert!(
            super::super::access::authorize(denied, &principal, &policy, &mut resources).is_err()
        );
        resources.refresh(Resources {
            projects: resources.projects.clone(),
            ..Default::default()
        });
        assert_eq!(
            resources
                .session("local-a")
                .unwrap()
                .source
                .as_ref()
                .unwrap()
                .source_id,
            "source-a"
        );
        assert!(resources.session("copied").is_none());
    }
    #[test]
    fn cloud_catalog_pages_outlive_the_bounded_permission_cache() {
        let principal = Principal {
            issuer: "https://cloud.example".into(),
            account_id: "reader".into(),
        };
        let policy = Policy::new(
            1,
            vec![Rule {
                principal: principal.clone(),
                resource: Resource::Project("project".into()),
                access: Access::Read,
            }],
        )
        .unwrap();
        let mut resources = Resources::default();
        resources
            .projects
            .insert("project".into(), "/workspace/project".into());
        let row = |n: usize| {
            json!({"session_ref":format!("local-{n:064x}"), "source_id":"source", "source_generation":1,
            "native_session_id":format!("native-{n}"), "runtime":"codex", "cwd":"/workspace/project"})
        };
        let first = row(0);
        for page in 0..20 {
            let mut result = json!({"rows":(page*500..(page+1)*500).map(row).collect::<Vec<_>>()});
            resources.observe("session.catalog.list", &result);
            resources.filter("session.catalog.list", &mut result, &policy, &principal);
            assert_eq!(result["rows"].as_array().unwrap().len(), 500);
            assert!(resources.catalog.len() <= CATALOG_CACHE_LIMIT);
        }
        let id = first["session_ref"].as_str().unwrap();
        assert!(resources.session(id).is_none());
        let mut revisited = json!({"rows":[first.clone()]});
        resources.filter("session.catalog.list", &mut revisited, &policy, &principal);
        assert_eq!(revisited["rows"].as_array().unwrap().len(), 1);
        let persisted = crate::rc::runtime_context::CatalogRow {
            session_ref: id.into(),
            source_id: "source".into(),
            source_generation: 1,
            runtime: "codex".into(),
            native_session_id: "native-0".into(),
            cwd: "/workspace/project".into(),
            title: None,
            gist: None,
            updated_at_ms: None,
            writer_active: None,
            parent_session_ref: None,
            technical: false,
        };
        let resolved = resources
            .resolve_catalog_with(id, |requested| {
                assert_eq!(requested, id);
                Ok(Some(persisted))
            })
            .unwrap()
            .unwrap();
        assert_eq!(resolved.access(&policy, &principal), Access::Read);
        assert!(resources.session(id).is_some());
        assert!(resources.catalog.len() <= CATALOG_CACHE_LIMIT);
    }
}
