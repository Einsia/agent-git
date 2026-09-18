//! Session delegation narrows Hub controller authority and fences replaced owners.

use super::{resources::Resources, store};
use agit_peer::{
    access::{Access, Policy, Principal, Resource, Rule},
    cloud::{ConnectionGrant, SessionController},
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
    scope: SessionController,
    owner: Owner,
    slot: Slot,
}

impl Owners {
    pub async fn accept(&self, grant: &ConnectionGrant) -> anyhow::Result<Option<Controller>> {
        let Some(scope) = &grant.session_controller else {
            return Ok(None);
        };
        let key = hex::encode(Sha256::digest(
            serde_json::json!([
                grant.target.owner.issuer,
                grant.target.id,
                grant.target.credential_epoch,
                scope.runtime,
                scope.session_id,
            ])
            .to_string(),
        ));
        let owner = Owner {
            generation: scope.generation,
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
        Ok(Some(Controller {
            scope: scope.clone(),
            owner,
            slot,
        }))
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
        "session controller ownership changed"
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
    pub fn current(&self) -> Option<RwLockReadGuard<'_, Owner>> {
        let current = self
            .slot
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (self.owner == *current).then_some(current)
    }

    pub fn canonical(&self, resources: &Resources) -> bool {
        resources
            .session(&self.scope.session_id)
            .is_some_and(|session| {
                session.runtime == self.scope.runtime
                    && if session.native_id.is_empty() {
                        session.id == self.scope.session_id
                    } else {
                        session.native_id == self.scope.session_id
                    }
            })
    }

    pub fn policy(&self, base: &Policy, resources: &Resources, principal: &Principal) -> Policy {
        let Some(session) = resources
            .session(&self.scope.session_id)
            .filter(|_| self.canonical(resources))
        else {
            return Policy::default();
        };
        let access = match (session.access(base, principal), self.scope.access) {
            (Access::Deny, _) | (_, Access::Deny) => Access::Deny,
            (Access::Read, _) | (_, Access::Read) => Access::Read,
            (Access::Control, _) | (_, Access::Control) => Access::Control,
            _ => Access::Admin,
        };
        Policy::new(
            base.revision(),
            vec![Rule {
                principal: principal.clone(),
                resource: Resource::Session(self.scope.session_id.clone()),
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
                        | "workspace.list"
                        | "session.list"
                        | "session.history"
                        | "session.goal.read"
                        | "session.subscribe"
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
        frame
            .caller
            .as_mut()
            .context("controller caller is missing")?
            .account_id = Some(serde_json::json!([actor.issuer, actor.account_id]).to_string());
        Ok(())
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
            scope: SessionController {
                session_id: "shared".into(),
                runtime: "codex".into(),
                generation: 1,
                access: Access::Control,
            },
            owner: owner.clone(),
            slot: Arc::new(Ownership {
                current: RwLock::new(owner),
                admission: Mutex::new(()),
            }),
        };
        let policy = controller.policy(&base, &resources, &principal);
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
