//! A cloud connection cannot inherit owner RPC authority or unfiltered fanout.

use super::{
    access::{self, Permit},
    delegation,
    resources::Resources,
    store,
};
use crate::protocol::{Frame, RequestId, RpcError};
use agit_peer::access::{Policy, Principal};
use anyhow::Context;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

#[derive(Default)]
struct State {
    policy: Policy,
    resources: Resources,
}

pub struct Registry {
    state: Arc<RwLock<State>>,
    owners: Arc<delegation::Owners>,
    task: tokio::task::JoinHandle<()>,
    log: Option<crate::rc::diagnostics::Log>,
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Registry {
    pub(in crate::rc) fn start(log: Option<crate::rc::diagnostics::Log>) -> Self {
        let state = Arc::new(RwLock::new(State::default()));
        let current = state.clone();
        let monitor = log.clone();
        let task = tokio::spawn(async move {
            let log = monitor;
            let mut healthy = None;
            let mut reload = tokio::time::interval(Duration::from_secs(1));
            reload.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                reload.tick().await;
                let loaded = tokio::task::spawn_blocking(|| {
                    Ok::<_, anyhow::Error>((store::policy()?, Resources::load()?))
                })
                .await;
                let mut state = current
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match loaded {
                    Ok(Ok((policy, resources))) => {
                        if (healthy != Some(true) || policy.revision() != state.policy.revision())
                            && let Some(log) = &log
                        {
                            log.record(
                                "cloud.policy_loaded",
                                serde_json::json!({"revision":policy.revision()}),
                            );
                        }
                        healthy = Some(true);
                        state.policy = policy;
                        state.resources.refresh(resources);
                    }
                    _ => {
                        if healthy != Some(false)
                            && let Some(log) = &log
                        {
                            log.record(
                                "cloud.policy_unavailable",
                                serde_json::json!({"action":"deny"}),
                            );
                        }
                        healthy = Some(false);
                        *state = State::default();
                    }
                }
            }
        });
        Self {
            state,
            owners: Default::default(),
            task,
            log,
        }
    }

    #[cfg(test)]
    pub fn fixed(policy: Policy) -> Self {
        Self {
            state: Arc::new(RwLock::new(State {
                policy,
                resources: Resources::default(),
            })),
            owners: Default::default(),
            task: tokio::spawn(std::future::pending()),
            log: None,
        }
    }

    pub fn client(&self, principal: Principal, expires_at_ms: i64) -> Client {
        Client {
            principal,
            controller: None,
            lease: Lease(Arc::new(RwLock::new(LeaseState::new(expires_at_ms)))),
            state: self.state.clone(),
            permits: Default::default(),
            live: Arc::new(RwLock::new(true)),
            log: self.log.clone(),
        }
    }

    pub fn admitted(
        &self,
        grant: agit_peer::cloud::ConnectionGrant,
    ) -> impl std::future::Future<Output = anyhow::Result<Client>> + Send + use<> {
        let mut client = self.client(grant.caller.clone(), grant.expires_at_ms);
        let (state, owners) = (self.state.clone(), self.owners.clone());
        async move {
            if let Some(scope) = &grant.session_controller {
                let state = state
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let session = state
                    .resources
                    .session(&scope.session_id)
                    .context("controller session is unavailable")?;
                anyhow::ensure!(
                    session.runtime == scope.runtime
                        && if session.native_id.is_empty() {
                            session.id == scope.session_id
                        } else {
                            session.native_id == scope.session_id
                        },
                    "controller requires a canonical executor session"
                );
            }
            client.controller = owners.accept(&grant).await?;
            Ok(client)
        }
    }
}

#[derive(Clone)]
pub(crate) struct Lease(Arc<RwLock<LeaseState>>);

struct LeaseState {
    expires_at_ms: i64,
    deadline: tokio::time::Instant,
}

impl LeaseState {
    fn new(expires_at_ms: i64) -> Self {
        let remaining = expires_at_ms
            .saturating_sub(chrono::Utc::now().timestamp_millis())
            .max(0);
        let now = tokio::time::Instant::now();
        let deadline = now
            .checked_add(Duration::from_millis(remaining as u64))
            .unwrap_or(now);
        Self {
            expires_at_ms,
            deadline,
        }
    }

    fn current(&self) -> bool {
        tokio::time::Instant::now() < self.deadline
            && chrono::Utc::now().timestamp_millis() < self.expires_at_ms
    }
}

impl Lease {
    pub fn deadline(&self) -> tokio::time::Instant {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .deadline
    }

    pub fn expires_at_ms(&self) -> i64 {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .expires_at_ms
    }

    fn current(&self) -> bool {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current()
    }

    pub fn renew(&self, expires_at_ms: i64) -> anyhow::Result<()> {
        let mut lease = self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        anyhow::ensure!(lease.current(), "expired cloud authority cannot be renewed");
        anyhow::ensure!(
            expires_at_ms > lease.expires_at_ms,
            "cloud lease must advance"
        );
        *lease = LeaseState::new(expires_at_ms);
        Ok(())
    }
}

#[derive(Clone)]
pub struct Client {
    pub principal: Principal,
    controller: Option<delegation::Controller>,
    lease: Lease,
    state: Arc<RwLock<State>>,
    permits: Arc<Mutex<HashMap<RequestId, Option<Permit>>>>,
    live: Arc<RwLock<bool>>,
    log: Option<crate::rc::diagnostics::Log>,
}

impl Drop for Client {
    fn drop(&mut self) {
        *self
            .live
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
    }
}

struct ExecutionAuthority {
    controller: Option<delegation::Controller>,
    lease: Lease,
    principal: Principal,
    role: String,
    permit: Permit,
    state: Arc<RwLock<State>>,
    live: Arc<RwLock<bool>>,
    request: RequestId,
    log: Option<crate::rc::diagnostics::Log>,
}

impl crate::rc::authority::Authority for ExecutionAuthority {
    fn admit(&self, accept: &mut dyn FnMut() -> bool) -> bool {
        let live = self
            .live
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let owner = self
            .controller
            .as_ref()
            .and_then(|controller| controller.current());
        let policy = delegation::policy(
            self.controller.as_ref(),
            &state.policy,
            &state.resources,
            &self.principal,
        );
        let allowed = (self.controller.is_none() || owner.is_some())
            && *live
            && self.lease.current()
            && self.permit.authority_matches(
                &state.resources,
                &policy,
                &self.principal,
                &self.role,
            );
        if !allowed && let Some(log) = &self.log {
            log.record("cloud.execution_rejected", serde_json::json!({"principal":self.principal,"request_id":self.request,"method":self.permit.method,"policy_revision":state.policy.revision(),"connected":*live,"grant_expires_at_ms":self.lease.expires_at_ms()}));
        }
        allowed && accept()
    }
}

#[derive(Debug)]
pub enum Rejection {
    Close,
    Reply(RpcError),
}

impl Client {
    fn current(&self) -> bool {
        *self
            .live
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            && self.lease.current()
            && self
                .controller
                .as_ref()
                .is_none_or(|controller| controller.current().is_some())
    }

    pub(crate) fn lease(&self) -> Lease {
        self.lease.clone()
    }

    pub fn authorize(&self, frame: Frame) -> Result<Frame, Rejection> {
        if !self.current() {
            return Err(Rejection::Close);
        }
        let mut permits = self
            .permits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // An error reply with a reused ID would consume the original request's permit.
        if permits.len() >= 64 || frame.id.as_ref().is_none_or(|id| permits.contains_key(id)) {
            return Err(Rejection::Close);
        }
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let State { policy, resources } = &mut *state;
        let id = frame.id.clone().unwrap();
        permits.insert(id.clone(), None);
        let owner = self
            .controller
            .as_ref()
            .and_then(|controller| controller.current());
        if self.controller.is_some() && owner.is_none() {
            return Err(Rejection::Close);
        }
        let effective =
            delegation::policy(self.controller.as_ref(), policy, resources, &self.principal);
        let (mut frame, permit) = access::authorize(frame, &self.principal, &effective, resources)
            .map_err(Rejection::Reply)?;
        if let Some(controller) = &self.controller {
            controller.actor(&mut frame, &self.principal).map_err(|_| {
                Rejection::Reply(RpcError::new(
                    crate::protocol::ErrorCode::Forbidden,
                    "controller actor is unavailable",
                ))
            })?;
        }
        frame.authority = crate::rc::authority::Guard::new(ExecutionAuthority {
            controller: self.controller.clone(),
            principal: self.principal.clone(),
            lease: self.lease.clone(),
            role: frame.caller.as_ref().unwrap().role.clone(),
            permit: permit.clone(),
            state: self.state.clone(),
            live: self.live.clone(),
            request: id.clone(),
            log: self.log.clone(),
        });
        permits.insert(id, Some(permit));
        Ok(frame)
    }

    pub fn project(&self, record: &str) -> crate::Result<Option<String>> {
        if !self.current() {
            return Ok(None);
        }
        let mut frame: Frame =
            serde_json::from_str(record).context("executor produced an invalid frame")?;
        let permit = frame.id.as_ref().and_then(|id| {
            self.permits
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(id)
        });
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let State { policy, resources } = &mut *state;
        let owner = self
            .controller
            .as_ref()
            .and_then(|controller| controller.current());
        if self.controller.is_some() && owner.is_none() {
            return Ok(None);
        }
        if let Some(Some(permit)) = &permit {
            if let Some(result) = &frame.result {
                permit.observe(resources, result);
            }
            let effective =
                delegation::policy(self.controller.as_ref(), policy, resources, &self.principal);
            frame = permit.response(frame, resources, &effective, &self.principal);
            if permit.method == "machine.describe"
                && let Some(result) = frame.result.as_mut()
            {
                result["authority"] = serde_json::json!(if self.controller.is_some() {
                    agit_peer::cloud::SESSION_CONTROLLER_AUTHORITY
                } else {
                    "cloud-principal"
                });
                result["access_ceiling"] = serde_json::json!(true);
                if let Some(result) = result.as_object_mut() {
                    result.remove("diagnostic_log");
                }
            }
            return Ok(Some(frame.to_json()));
        }
        if frame.id.is_some() {
            return Ok(matches!(permit, Some(None)).then(|| frame.to_json()));
        }
        let effective =
            delegation::policy(self.controller.as_ref(), policy, resources, &self.principal);
        Ok(
            access::event_allowed(&frame, resources, &effective, &self.principal)
                .then(|| frame.to_json()),
        )
    }

    pub fn receipt_key(&self, key: String, frame: &Frame) -> String {
        if self.controller.is_some() {
            serde_json::json!([
                "session-controller",
                self.principal.issuer,
                frame
                    .caller
                    .as_ref()
                    .and_then(|caller| caller.account_id.as_deref()),
                key
            ])
            .to_string()
        } else {
            serde_json::json!([self.principal, key]).to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agit_peer::access::{Access, Resource, Rule};
    use serde_json::json;

    fn principal() -> Principal {
        Principal {
            issuer: "https://cloud.example".into(),
            account_id: "reader".into(),
        }
    }

    fn policy() -> Policy {
        Policy::new(
            1,
            vec![Rule {
                principal: principal(),
                resource: Resource::Session("visible".into()),
                access: Access::Read,
            }],
        )
        .unwrap()
    }

    #[tokio::test]
    async fn revoked_or_disconnected_work_cannot_be_taken_from_the_executor_queue() {
        use crate::rc::ticket::{Abandon, ticket_authorized};
        let registry = Registry::fixed(policy());
        let client = registry.client(principal(), i64::MAX);
        registry.state.write().unwrap().resources.observe(
            "session.list",
            &json!({"local":[{"runtime_session_id":"visible","runtime":"codex","cwd":"/trusted"}]}),
        );
        let frame = client
            .authorize(Frame::request(
                "session.history",
                json!({"session_id":"visible"}),
            ))
            .unwrap();
        let (accepted, receipt) = ticket_authorized::<()>(frame.authority.clone());
        assert!(accepted.accept());
        registry.state.write().unwrap().policy = Policy::default();
        assert_eq!(receipt.abandon(), Abandon::AlreadyTaken);
        let (queued, receipt) = ticket_authorized::<()>(frame.authority.clone());
        assert!(!queued.accept());
        assert_eq!(receipt.abandon(), Abandon::NeverRan);
        registry.state.write().unwrap().policy = policy();
        let (queued, receipt) = ticket_authorized::<()>(frame.authority.clone());
        drop(client);
        assert!(!queued.accept());
        assert_eq!(receipt.abandon(), Abandon::NeverRan);
    }

    #[tokio::test(start_paused = true)]
    async fn renewed_authority_reaches_queued_work_but_cannot_resurrect_an_expired_lease() {
        use crate::rc::ticket::ticket_authorized;
        let registry = Registry::fixed(policy());
        registry.state.write().unwrap().resources.observe(
            "session.list",
            &json!({"local":[{"runtime_session_id":"visible","runtime":"codex","cwd":"/trusted"}]}),
        );
        let now = chrono::Utc::now().timestamp_millis();
        let client = registry.client(principal(), now + 60_000);
        let frame = client
            .authorize(Frame::request(
                "session.history",
                json!({"session_id":"visible"}),
            ))
            .unwrap();
        let (ticket, _) = ticket_authorized::<()>(frame.authority.clone());
        tokio::time::advance(Duration::from_secs(30)).await;
        client.lease().renew(now + 120_000).unwrap();
        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(ticket.accept());
        tokio::time::advance(Duration::from_secs(90)).await;
        assert!(client.lease().renew(now + 240_000).is_err());
        let (ticket, _) = ticket_authorized::<()>(frame.authority.clone());
        assert!(!ticket.accept());
    }

    #[tokio::test(start_paused = true)]
    async fn live_connections_cannot_read_write_or_release_queued_work_after_grant_expiry() {
        use crate::rc::ticket::{Abandon, ticket_authorized};
        let registry = Registry::fixed(
            Policy::new(
                1,
                vec![Rule {
                    principal: principal(),
                    resource: Resource::Session("visible".into()),
                    access: Access::Control,
                }],
            )
            .unwrap(),
        );
        registry.state.write().unwrap().resources.observe(
            "session.list",
            &json!({"local":[{"runtime_session_id":"visible","runtime":"codex","cwd":"/trusted"}]}),
        );
        let client = registry.client(principal(), chrono::Utc::now().timestamp_millis() + 60_000);
        let request = Frame::request("turn.start", json!({"session_id":"visible"}));
        let queued = client.authorize(request.clone()).unwrap();
        let (ticket, receipt) = ticket_authorized::<()>(queued.authority.clone());
        let read = client
            .authorize(Frame::request(
                "session.history",
                json!({"session_id":"visible"}),
            ))
            .unwrap();
        let response = Frame::response(read.id.unwrap(), json!({"items":["private"]}));
        let mut event = Frame::notification("item.completed", json!({"text":"private"}));
        event.stream = Some("visible".into());
        assert!(client.project(&event.to_json()).unwrap().is_some());
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(*client.live.read().unwrap());
        assert!(!ticket.accept());
        assert_eq!(receipt.abandon(), Abandon::NeverRan);
        assert!(matches!(
            client.authorize(request.clone()),
            Err(Rejection::Close)
        ));
        assert!(matches!(
            client.authorize(Frame::request(
                "session.history",
                json!({"session_id":"visible"})
            )),
            Err(Rejection::Close)
        ));
        assert!(client.project(&event.to_json()).unwrap().is_none());
        assert!(client.project(&response.to_json()).unwrap().is_none());
        let renewed = registry.client(principal(), chrono::Utc::now().timestamp_millis() + 60_000);
        assert!(
            renewed
                .authorize(request)
                .unwrap()
                .authority
                .check()
                .is_ok()
        );
        assert!(queued.authority.check().is_err());
    }

    #[tokio::test]
    async fn request_ceiling_survives_queue_admission_without_elevating_policy() {
        let rule = |access| Rule {
            principal: principal(),
            resource: Resource::Session("visible".into()),
            access,
        };
        let registry = Registry::fixed(Policy::new(1, vec![rule(Access::Admin)]).unwrap());
        let client = registry.client(principal(), i64::MAX);
        registry.state.write().unwrap().resources.observe(
            "session.list",
            &json!({"local":[{"runtime_session_id":"visible","runtime":"codex","cwd":"/trusted"}]}),
        );
        let request = |ceiling| {
            Frame::request(
                "turn.start",
                json!({"session_id":"visible","access_ceiling":ceiling}),
            )
        };
        let queued = client.authorize(request("control")).unwrap();
        assert_eq!(queued.caller.as_ref().unwrap().role, "operator");
        assert!(
            queued
                .params
                .as_ref()
                .unwrap()
                .get("access_ceiling")
                .is_none()
        );
        assert!(queued.authority.check().is_ok());
        assert!(client.authorize(request("read")).is_err());
        assert!(client.authorize(request("invalid")).is_err());

        registry.state.write().unwrap().policy =
            Policy::new(2, vec![rule(Access::Control)]).unwrap();
        assert!(queued.authority.check().is_ok());
        let bounded = client.authorize(request("admin")).unwrap();
        assert_eq!(bounded.caller.as_ref().unwrap().role, "operator");
        registry.state.write().unwrap().policy = Policy::new(3, vec![rule(Access::Read)]).unwrap();
        assert!(queued.authority.check().is_err());
        assert!(bounded.authority.check().is_err());
    }

    #[tokio::test]
    async fn a_role_downgrade_cannot_reuse_queued_owner_authority() {
        let rule = |access| Rule {
            principal: principal(),
            resource: Resource::Session("visible".into()),
            access,
        };
        let registry = Registry::fixed(Policy::new(1, vec![rule(Access::Admin)]).unwrap());
        let client = registry.client(principal(), i64::MAX);
        registry.state.write().unwrap().resources.observe(
            "session.list",
            &json!({"local":[{"runtime_session_id":"visible","runtime":"codex","cwd":"/trusted"}]}),
        );
        let request = Frame::request("turn.start", json!({"session_id":"visible"}));
        let frame = client.authorize(request).unwrap();
        assert_eq!(frame.caller.as_ref().unwrap().role, "owner");
        assert!(frame.authority.check().is_ok());
        registry.state.write().unwrap().policy =
            Policy::new(2, vec![rule(Access::Control)]).unwrap();
        assert!(frame.authority.check().is_err());
        let fresh = client
            .authorize(Frame::request(
                "turn.start",
                json!({"session_id":"visible"}),
            ))
            .unwrap();
        assert_eq!(fresh.caller.as_ref().unwrap().role, "operator");
        assert!(fresh.authority.check().is_ok());
        let decoded: Frame = serde_json::from_str(&frame.to_json()).unwrap();
        assert!(decoded.authority.check().is_ok());
        assert!(!frame.to_json().contains("authority"));
    }

    #[tokio::test]
    async fn active_and_rejected_ids_remain_reserved_until_their_own_response() {
        let registry = Registry::fixed(policy());
        let client = registry.client(principal(), i64::MAX);
        let request = Frame::request("machine.describe", json!({}));
        client.authorize(request.clone()).unwrap();
        assert!(matches!(
            client.authorize(request.clone()),
            Err(Rejection::Close)
        ));
        let response = Frame::response(
            request.id.clone().unwrap(),
            json!({"authority":"local-owner","diagnostic_log":"private-path"}),
        );
        let projected: Frame =
            serde_json::from_str(&client.project(&response.to_json()).unwrap().unwrap()).unwrap();
        let result = projected.result.unwrap();
        assert_eq!(result["authority"], "cloud-principal");
        assert!(result.get("diagnostic_log").is_none());
        assert!(client.project(&response.to_json()).unwrap().is_none());

        let rejected = Frame::request("peer.list", json!({}));
        let Err(Rejection::Reply(error)) = client.authorize(rejected.clone()) else {
            panic!("peer management must be denied")
        };
        let mut replacement = request;
        replacement.id = rejected.id.clone();
        assert!(matches!(
            client.authorize(replacement.clone()),
            Err(Rejection::Close)
        ));
        let denial = Frame::error_response(rejected.id.unwrap(), error);
        assert!(client.project(&denial.to_json()).unwrap().is_some());
        client.authorize(replacement).unwrap();
    }

    #[tokio::test]
    async fn queued_results_and_events_use_current_policy() {
        let registry = Registry::fixed(policy());
        let client = registry.client(principal(), i64::MAX);
        let list = Frame::request("session.list", json!({}));
        client.authorize(list.clone()).unwrap();
        let result = json!({"sessions":[], "local":[
            {"runtime_session_id":"visible","runtime":"codex","cwd":"/trusted"},
            {"runtime_session_id":"hidden","runtime":"codex","cwd":"/private"}
        ]});
        let projected = client
            .project(&Frame::response(list.id.unwrap(), result).to_json())
            .unwrap()
            .unwrap();
        let projected: Frame = serde_json::from_str(&projected).unwrap();
        assert_eq!(
            projected.result.unwrap()["local"].as_array().unwrap().len(),
            1
        );
        let read = Frame::request("session.history", json!({"session_id":"visible"}));
        client.authorize(read.clone()).unwrap();
        let mut event = Frame::notification("item.completed", json!({"text":"private transcript"}));
        event.stream = Some("visible".into());
        assert!(client.project(&event.to_json()).unwrap().is_some());
        registry.state.write().unwrap().policy = Policy::default();
        let response = Frame::response(read.id.unwrap(), json!({"items":["private transcript"]}));
        let projected = client.project(&response.to_json()).unwrap().unwrap();
        assert!(!projected.contains("private transcript"));
        assert!(
            serde_json::from_str::<Frame>(&projected)
                .unwrap()
                .error
                .is_some()
        );
        assert!(client.project(&event.to_json()).unwrap().is_none());
    }
}
