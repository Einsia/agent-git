//! Delegation narrows Hub controller authority and fences replaced owners.

use super::{resources::Resources, store};
use agit_peer::{
    access::{Access, Policy, Principal, Resource, Rule},
    cloud::{ConnectionGrant, ProjectController, SessionController},
};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, RwLock, RwLockReadGuard, Weak},
};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Owner {
    generation: u64,
    source: String,
}

struct Ownership {
    current: RwLock<Owner>,
    admission: Mutex<()>,
}

type Slot = Arc<Ownership>;

#[derive(Default)]
pub(super) struct Owners(Mutex<HashMap<String, Weak<Ownership>>>);

#[derive(Clone)]
pub(super) struct Controller {
    scope: Scope,
    owner: Owner,
    slot: Slot,
}

#[derive(Clone)]
enum Scope {
    Session(SessionController),
    Project(ProjectController),
}

impl Scope {
    fn from_grant(grant: &ConnectionGrant) -> anyhow::Result<Option<Self>> {
        match (&grant.session_controller, &grant.project_controller) {
            (Some(session), None) => Ok(Some(Self::Session(session.clone()))),
            (None, Some(project)) => Ok(Some(Self::Project(project.clone()))),
            (None, None) => Ok(None),
            _ => anyhow::bail!("controller grant has conflicting resource scopes"),
        }
    }

    fn canonical(&self, resources: &Resources) -> bool {
        match self {
            Self::Session(scope) => resources.session(&scope.session_id).is_some_and(|session| {
                session.runtime == scope.runtime
                    && if session.native_id.is_empty() {
                        session.id == scope.session_id
                    } else {
                        session.native_id == scope.session_id
                    }
            }),
            Self::Project(scope) => {
                Path::new(&scope.local_path).is_absolute()
                    && resources
                        .projects
                        .get(&scope.project_id)
                        .is_some_and(|path| path == Path::new(&scope.local_path))
            }
        }
    }
}

pub(super) fn validate_resources(
    grant: &ConnectionGrant,
    resources: &Resources,
) -> anyhow::Result<()> {
    ensure!(
        Scope::from_grant(grant)?.is_none_or(|scope| scope.canonical(resources)),
        "controller resource is unavailable or changed"
    );
    Ok(())
}

impl Owners {
    pub async fn accept(&self, grant: &ConnectionGrant) -> anyhow::Result<Option<Controller>> {
        let Some(scope) = Scope::from_grant(grant)? else {
            return Ok(None);
        };
        let (coordinate, generation) = match &scope {
            Scope::Session(scope) => (
                serde_json::json!([
                    grant.target.owner.issuer,
                    grant.target.id,
                    grant.target.credential_epoch,
                    scope.runtime,
                    scope.session_id,
                ]),
                scope.generation,
            ),
            Scope::Project(scope) => (
                serde_json::json!([
                    "project-controller",
                    grant.target.owner.issuer,
                    grant.target.id,
                    grant.target.credential_epoch,
                    scope.project_id,
                ]),
                scope.generation,
            ),
        };
        let key = hex::encode(Sha256::digest(coordinate.to_string()));
        let owner = Owner {
            generation,
            source: serde_json::json!([
                grant.source.id,
                grant.source.credential_epoch,
                grant.source.certificate.fingerprint()
            ])
            .to_string(),
        };
        // Pending admissions retain the ownership boundary even when every connection closes.
        let slot = self.slot(&key, &owner);
        let (pending, file_owner) = (slot.clone(), owner.clone());
        tokio::task::spawn_blocking(move || {
            let path = super::super::rc_dir()?.join(format!("cloud-session-owner-{key}.json"));
            pending.publish(&path, &file_owner)
        })
        .await??;
        Ok(Some(Controller { scope, owner, slot }))
    }

    fn slot(&self, key: &str, owner: &Owner) -> Slot {
        let mut entries = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|_, slot| slot.strong_count() > 0);
        entries.get(key).and_then(Weak::upgrade).unwrap_or_else(|| {
            let slot = Arc::new(Ownership {
                current: RwLock::new(owner.clone()),
                admission: Mutex::new(()),
            });
            entries.insert(key.to_owned(), Arc::downgrade(&slot));
            slot
        })
    }
}

impl Ownership {
    fn publish(&self, path: &Path, owner: &Owner) -> anyhow::Result<()> {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pin_owner(path, owner)?;
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        advance(&current, owner)?;
        *current = owner.clone();
        Ok(())
    }
}

fn advance(current: &Owner, next: &Owner) -> anyhow::Result<()> {
    ensure!(
        next.generation > current.generation || next == current,
        "controller ownership changed"
    );
    Ok(())
}

fn pin_owner(path: &Path, owner: &Owner) -> anyhow::Result<()> {
    let lock = store::private_lock(&path.with_extension("lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    if let Some(current) = store::read::<Owner>(path, 4096)? {
        advance(&current, owner)?;
        if current == *owner {
            return Ok(());
        }
    }
    store::write(path, owner)
}

impl Controller {
    pub fn watch_owner(&self) -> Option<String> {
        let Scope::Session(scope) = &self.scope else {
            return None;
        };
        Some(
            serde_json::json!([
                scope.runtime,
                scope.session_id,
                self.owner.generation,
                self.owner.source
            ])
            .to_string(),
        )
    }

    pub fn project(&self) -> Option<(&str, &Path)> {
        match &self.scope {
            Scope::Project(scope) => Some((&scope.project_id, Path::new(&scope.local_path))),
            Scope::Session(_) => None,
        }
    }

    pub fn current(&self) -> Option<RwLockReadGuard<'_, Owner>> {
        let current = self
            .slot
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (self.owner == *current).then_some(current)
    }

    pub fn canonical(&self, resources: &Resources) -> bool {
        self.scope.canonical(resources)
    }

    pub fn authority(&self) -> &'static str {
        match self.scope {
            Scope::Session(_) => agit_peer::cloud::SESSION_CONTROLLER_AUTHORITY,
            Scope::Project(_) => agit_peer::cloud::PROJECT_CONTROLLER_AUTHORITY,
        }
    }

    pub fn has_session_stream(&self) -> bool {
        matches!(self.scope, Scope::Session(_))
    }

    pub fn policy(&self, base: &Policy, resources: &Resources, principal: &Principal) -> Policy {
        if let Scope::Project(scope) = &self.scope {
            if !self.canonical(resources) {
                return Policy::default();
            }
            let access = intersect(
                base.access(principal, None, Some(&scope.project_id)),
                scope.access,
            );
            // Equivalent directory bindings share discovery; execution retains the granted ID.
            let aliases: std::collections::HashSet<_> = resources
                .projects
                .iter()
                .filter(|(_, path)| path.as_path() == Path::new(&scope.local_path))
                .map(|(id, _)| id.as_str())
                .collect();
            let mut rules: Vec<_> = aliases
                .iter()
                .map(|id| Rule {
                    principal: principal.clone(),
                    resource: Resource::Project((*id).into()),
                    access: intersect(base.access(principal, None, Some(id)), access),
                })
                .collect();
            for rule in base
                .rules()
                .iter()
                .filter(|rule| &rule.principal == principal)
            {
                if let Resource::Session(id) = &rule.resource
                    && let Some(session) = resources.session(id).filter(|session| {
                        session
                            .project
                            .as_deref()
                            .is_some_and(|id| aliases.contains(id))
                    })
                {
                    rules.push(Rule {
                        principal: principal.clone(),
                        resource: rule.resource.clone(),
                        access: intersect(session.access(base, principal), access),
                    });
                }
            }
            return Policy::new(base.revision(), rules).unwrap_or_default();
        }
        let Scope::Session(scope) = &self.scope else {
            unreachable!()
        };
        let Some(session) = resources
            .session(&scope.session_id)
            .filter(|_| self.canonical(resources))
        else {
            return Policy::default();
        };
        let access = intersect(session.access(base, principal), scope.access);
        Policy::new(
            base.revision(),
            vec![Rule {
                principal: principal.clone(),
                resource: Resource::Session(scope.session_id.clone()),
                access,
            }],
        )
        .expect("delegation scope is validated at admission")
    }

    pub fn actor(
        &self,
        frame: &mut crate::protocol::Frame,
        principal: &Principal,
    ) -> anyhow::Result<()> {
        ensure!(
            self.has_session_stream()
                || matches!(
                    frame.method(),
                    "machine.describe"
                        | "workspace.list"
                        | "session.list"
                        | "runtime.models"
                        | "session.start"
                ),
            "project controller cannot issue session or machine commands"
        );
        let actor = frame
            .params
            .as_mut()
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|params| params.remove("controller_actor"));
        let Some(actor) = actor else {
            ensure!(
                matches!(
                    frame.method(),
                    "machine.describe"
                        | "runtime.models"
                        | "workspace.list"
                        | "session.list"
                        | "session.history"
                        | "session.goal.read"
                        | "session.subscribe"
                        | "session.watch"
                        | "session.unwatch"
                        | "session.commands"
                        | "session.model"
                ),
                "controller mutations require an authenticated actor"
            );
            return Ok(());
        };
        let actor: Principal = serde_json::from_value(actor).context("invalid controller actor")?;
        ensure!(
            actor.issuer == principal.issuer
                && !actor.account_id.is_empty()
                && actor.account_id.len() <= 1024
                && !actor.account_id.chars().any(char::is_control),
            "invalid controller actor"
        );
        if frame.method() == "session.start"
            && let Some(params) = frame.params.as_mut()
            && let Some(id) = params["start_id"].as_str()
        {
            params["start_id"] = super::access::launch_key(&actor, id).into();
        }
        frame
            .caller
            .as_mut()
            .context("controller caller is missing")?
            .account_id = Some(serde_json::json!([actor.issuer, actor.account_id]).to_string());
        Ok(())
    }
}

fn intersect(first: Access, second: Access) -> Access {
    match (first, second) {
        (Access::Deny, _) | (_, Access::Deny) => Access::Deny,
        (Access::Read, _) | (_, Access::Read) => Access::Read,
        (Access::Control, _) | (_, Access::Control) => Access::Control,
        _ => Access::Admin,
    }
}

pub(super) fn policy<'a>(
    controller: Option<&Controller>,
    base: &'a Policy,
    resources: &Resources,
    principal: &Principal,
) -> Cow<'a, Policy> {
    controller.map_or(Cow::Borrowed(base), |controller| {
        Cow::Owned(controller.policy(base, resources, principal))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{protocol::Frame, rc::cloud::access};
    use serde_json::json;

    #[test]
    fn project_delegation_discovers_and_creates_without_controlling_session_streams() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared");
        let other = directory.path().join("other");
        let principal = Principal {
            issuer: "https://cloud.example".into(),
            account_id: "owner".into(),
        };
        let base = Policy::new(
            1,
            vec![
                Rule {
                    principal: principal.clone(),
                    resource: Resource::Machine,
                    access: Access::Admin,
                },
                Rule {
                    principal: principal.clone(),
                    resource: Resource::Session("hidden".into()),
                    access: Access::Deny,
                },
            ],
        )
        .unwrap();
        let mut resources = Resources::default();
        resources.observe(
            "workspace.list",
            &json!({"workspaces":[{"workspace_id":"local-owner","projects":[
                {"project_id":"alias","local_path":path}, {"project_id":"other","local_path":other}
            ]}]}),
        );
        let catalog = json!({"local":[
            {"runtime_session_id":"visible","runtime":"codex","cwd":path},
            {"runtime_session_id":"hidden","runtime":"codex","cwd":path},
            {"runtime_session_id":"other","runtime":"codex","cwd":other}
        ]});
        resources.observe("session.list", &catalog);
        resources.observe(
            "project.bind",
            &json!({"project_id":"shared","local_path":path}),
        );
        let owner = Owner {
            generation: 1,
            source: "controller".into(),
        };
        let controller = Controller {
            scope: Scope::Project(ProjectController {
                project_id: "shared".into(),
                local_path: path.to_string_lossy().into(),
                generation: 1,
                access: Access::Control,
            }),
            owner: owner.clone(),
            slot: Arc::new(Ownership {
                current: RwLock::new(owner),
                admission: Mutex::new(()),
            }),
        };
        let policy = controller.policy(&base, &resources, &principal);
        assert_eq!(controller.project(), Some(("shared", path.as_path())));
        let mut filtered = catalog;
        resources.filter("session.list", &mut filtered, &policy, &principal);
        assert_eq!(filtered["local"].as_array().unwrap().len(), 1);
        assert_eq!(filtered["local"][0]["runtime_session_id"], "visible");
        let (mut create, _) = access::authorize(
            Frame::request(
                "session.start",
                json!({"project_id":"shared","start_id":uuid::Uuid::new_v4().to_string(),"controller_actor":{
                    "issuer":"https://cloud.example","account_id":"member"
                }}),
            ),
            &principal,
            &policy,
            &mut resources,
        )
        .unwrap();
        let original = create.clone();
        controller.actor(&mut create, &principal).unwrap();
        let mut retry = original.clone();
        controller.actor(&mut retry, &principal).unwrap();
        assert_eq!(
            create.params.as_ref().unwrap()["start_id"],
            retry.params.as_ref().unwrap()["start_id"]
        );
        let mut other_member = original.clone();
        other_member.params.as_mut().unwrap()["controller_actor"]["account_id"] =
            "other-member".into();
        controller.actor(&mut other_member, &principal).unwrap();
        assert_ne!(
            create.params.as_ref().unwrap()["start_id"],
            other_member.params.as_ref().unwrap()["start_id"]
        );
        let mut replacement = controller.clone();
        replacement.owner.generation += 1;
        let mut rejoined = original;
        replacement.actor(&mut rejoined, &principal).unwrap();
        assert_eq!(
            create.params.as_ref().unwrap()["start_id"],
            rejoined.params.as_ref().unwrap()["start_id"]
        );
        assert_eq!(create.caller.as_ref().unwrap().role, "operator");
        assert_eq!(
            create.caller.as_ref().unwrap().account_id.as_deref(),
            Some("[\"https://cloud.example\",\"member\"]")
        );
        let (mut subscribe, _) = access::authorize(
            Frame::request("session.subscribe", json!({"session_id":"visible"})),
            &principal,
            &policy,
            &mut resources,
        )
        .unwrap();
        assert!(controller.actor(&mut subscribe, &principal).is_err());
        assert!(!controller.has_session_stream());
        assert!(
            access::authorize(
                Frame::request("fs.readFile", json!({"path":path})),
                &principal,
                &policy,
                &mut resources
            )
            .is_err()
        );
        assert!(
            access::authorize(
                Frame::request("session.start", json!({"project_id":"other"})),
                &principal,
                &policy,
                &mut resources
            )
            .is_err()
        );
        resources.projects.insert("shared".into(), other);
        assert!(!controller.canonical(&resources));
        assert_eq!(
            controller.policy(&base, &resources, &principal).access(
                &principal,
                None,
                Some("shared")
            ),
            Access::Deny
        );
    }

    #[test]
    fn pending_admission_cannot_revive_after_its_replacement_disconnects() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owner.json");
        let owners = Owners::default();
        let old = Owner {
            generation: 1,
            source: "old".into(),
        };
        let new = Owner {
            generation: 2,
            source: "new".into(),
        };
        let pending = owners.slot("session", &old);
        pending.publish(&path, &old).unwrap();
        let replacement = owners.slot("session", &new);
        replacement.publish(&path, &new).unwrap();
        drop(replacement);
        assert!(pending.publish(&path, &old).is_err());
        assert_eq!(pending.current.read().unwrap().generation, 2);
        drop(pending);
        assert!(owners.slot("session", &old).publish(&path, &old).is_err());
        assert_eq!(
            store::read::<Owner>(&path, 4096)
                .unwrap()
                .unwrap()
                .generation,
            2
        );
    }

    #[test]
    fn delegated_owner_is_session_scoped_and_replacement_fences_its_commands() {
        let principal = Principal {
            issuer: "https://cloud.example".into(),
            account_id: "owner".into(),
        };
        let base = Policy::new(
            1,
            vec![Rule {
                principal: principal.clone(),
                resource: Resource::Machine,
                access: Access::Admin,
            }],
        )
        .unwrap();
        let mut resources = Resources::default();
        resources.observe(
            "session.list",
            &json!({"local":[
                {"runtime_session_id":"shared","runtime":"codex","cwd":"/project"},
                {"runtime_session_id":"private","runtime":"codex","cwd":"/project"}
            ]}),
        );
        let owner = Owner {
            generation: 1,
            source: "controller-a".into(),
        };
        let controller = Controller {
            scope: Scope::Session(SessionController {
                session_id: "shared".into(),
                runtime: "codex".into(),
                generation: 1,
                access: Access::Control,
            }),
            owner: owner.clone(),
            slot: Arc::new(Ownership {
                current: RwLock::new(owner),
                admission: Mutex::new(()),
            }),
        };
        let policy = controller.policy(&base, &resources, &principal);
        let watch_owner = controller.watch_owner().unwrap();
        assert_eq!(watch_owner, controller.clone().watch_owner().unwrap());
        let mut replacement = controller.clone();
        replacement.owner.generation += 1;
        assert_ne!(watch_owner, replacement.watch_owner().unwrap());
        for method in ["session.watch", "session.unwatch"] {
            let (mut frame, _) = access::authorize(
                Frame::request(method, json!({"session_id":"shared"})),
                &principal,
                &policy,
                &mut resources,
            )
            .unwrap();
            controller.actor(&mut frame, &principal).unwrap();
        }
        let (mut command, _) = access::authorize(Frame::request("turn.start", json!({"session_id":"shared","controller_actor":{"issuer":"https://cloud.example","account_id":"operator"}})), &principal, &policy, &mut resources).unwrap();
        controller.actor(&mut command, &principal).unwrap();
        assert_eq!(command.caller.as_ref().unwrap().role, "operator");
        assert_eq!(
            command.caller.as_ref().unwrap().account_id.as_deref(),
            Some("[\"https://cloud.example\",\"operator\"]")
        );
        assert!(
            command
                .params
                .as_ref()
                .unwrap()
                .get("controller_actor")
                .is_none()
        );
        assert!(
            access::authorize(
                Frame::request("turn.start", json!({"session_id":"private"})),
                &principal,
                &policy,
                &mut resources
            )
            .is_err()
        );
        assert!(
            access::authorize(
                Frame::request("fs.readFile", json!({"path":"/project/private"})),
                &principal,
                &policy,
                &mut resources
            )
            .is_err()
        );
        let mut rows =
            json!({"local":[{"runtime_session_id":"shared"},{"runtime_session_id":"private"}]});
        resources.filter("session.list", &mut rows, &policy, &principal);
        assert_eq!(rows["local"], json!([{"runtime_session_id":"shared"}]));
        assert!(controller.current().is_some());
        let replacement = Owner {
            generation: 2,
            source: "controller-b".into(),
        };
        advance(&controller.owner, &replacement).unwrap();
        *controller.slot.current.write().unwrap() = replacement.clone();
        assert!(controller.current().is_none());
        assert!(advance(&replacement, &controller.owner).is_err());
        assert!(
            advance(
                &replacement,
                &Owner {
                    generation: 2,
                    source: "controller-c".into()
                }
            )
            .is_err()
        );
    }
}
