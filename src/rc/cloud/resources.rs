//! Resource coordinates come from executor records and executor-produced catalogs.

use agit_peer::access::{Access, Policy, Principal, Resource};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub native_id: String,
    pub runtime: String,
    pub cwd: String,
    pub project: Option<String>,
}

impl Session {
    pub fn access(&self, policy: &Policy, principal: &Principal) -> Access {
        let aliases = [&self.id, &self.native_id];
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
}

impl Resources {
    pub fn refresh(&mut self, current: Self) {
        self.projects = current.projects;
        let projects = self.projects.clone();
        for session in self.sessions.values_mut() {
            if let Some(known) = current
                .sessions
                .get(&session.id)
                .or_else(|| current.sessions.get(&session.native_id))
            {
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
            resources.insert(Session {
                id,
                native_id: entry.thread_id,
                runtime: entry.runtime,
                cwd: entry.cwd,
                project: entry.project_id,
            });
        }
        Ok(resources)
    }

    fn insert(&mut self, session: Session) {
        if self.sessions.len() >= 8192 {
            return;
        }
        if !session.native_id.is_empty() {
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
                || (!source.native_id.is_empty() && known.native_id == source.native_id)
            {
                *known = session.clone();
            }
        }
        self.insert(session);
    }

    pub fn session(&self, id: &str) -> Option<&Session> {
        self.sessions.get(id)
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
                                || self.sessions.values().any(|session| {
                                    session.project.as_deref() == Some(id)
                                        && session.access(policy, principal).can_read()
                                })
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
}
