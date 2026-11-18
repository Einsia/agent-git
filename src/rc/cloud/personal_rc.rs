//! Verified launch scopes remain revocable while their requests wait for execution.

use super::{access::launch_key, host::Renewal, ingress::Lease};
use crate::protocol::{CallerClaim, Frame, SessionStart};
use agit_peer::{cloud::ConnectionGrant, personal_rc::StartScope};
use anyhow::{Context, ensure};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock, RwLock, Weak},
};

type Live = RwLock<bool>;
type Observations = Mutex<HashMap<String, Weak<Live>>>;

fn observation(grant: &ConnectionGrant, id: &str) -> Arc<Live> {
    static OBSERVATIONS: OnceLock<Observations> = OnceLock::new();
    let mut entries = OBSERVATIONS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    entries.retain(|_, entry| entry.strong_count() > 0);
    let key = serde_json::json!([grant.caller, grant.source.id, grant.target.id, id]).to_string();
    let entry = entries.entry(key).or_default();
    if let Some(live) = entry.upgrade() {
        return live;
    }
    let live = Arc::new(RwLock::new(true));
    *entry = Arc::downgrade(&live);
    live
}

#[derive(Clone)]
pub(crate) struct Proof {
    scope: StartScope,
    caller: String,
    project_path: std::path::PathBuf,
    live: Arc<Live>,
    expires_at_ms: i64,
    deadline: tokio::time::Instant,
}

impl Proof {
    #[cfg(test)]
    pub(crate) fn fixture(start: &SessionStart, caller: &CallerClaim) -> Self {
        Self {
            scope: StartScope {
                authorization_id: uuid::Uuid::new_v4().to_string(),
                project_id: start.project_id.clone(),
                runtime: start.runtime.clone(),
                start_id: start.start_id.clone().unwrap(),
                permission_mode: serde_json::to_value(start.permission_mode.unwrap())
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .into(),
            },
            caller: caller.account_id.clone().unwrap(),
            project_path: "/project".into(),
            live: Arc::new(RwLock::new(true)),
            expires_at_ms: i64::MAX,
            deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(60),
        }
    }

    pub(crate) fn admit(&self, accept: &mut dyn FnMut() -> bool) -> bool {
        let live = self
            .live
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *live
            && chrono::Utc::now().timestamp_millis() < self.expires_at_ms
            && tokio::time::Instant::now() < self.deadline
            && accept()
    }

    pub(crate) fn matches(&self, start: &SessionStart, caller: &CallerClaim) -> bool {
        caller.account_id.as_deref() == Some(&self.caller)
            && caller.role == "operator"
            && start.workspace_id == crate::rc::endpoint::WORKSPACE
            && start.project_id == self.scope.project_id
            && start.runtime == self.scope.runtime
            && start.start_id.as_deref() == Some(self.scope.start_id.as_str())
            && start.origin.is_none()
            && start.permission_mode.is_some_and(|mode| {
                serde_json::to_value(mode)
                    .ok()
                    .as_ref()
                    .and_then(|value| value.as_str())
                    == Some(self.scope.permission_mode.as_str())
            })
    }

    pub(crate) fn matches_project(&self, id: &str, path: &std::path::Path) -> bool {
        self.scope.project_id == id && self.project_path == path
    }
}

pub(in crate::rc) async fn verify(
    frame: &mut Frame,
    renewal: Option<&Renewal>,
    grant: &ConnectionGrant,
    lease: &Lease,
) -> anyhow::Result<()> {
    let Some(id) = frame
        .params
        .as_ref()
        .and_then(|params| params.get("personal_rc_authorization"))
    else {
        return Ok(());
    };
    let id = id
        .as_str()
        .context("invalid personal RC authorization ID")?;
    let id = uuid::Uuid::parse_str(id)
        .context("invalid personal RC authorization ID")?
        .to_string();
    let live = observation(grant, &id);
    let result = async {
        ensure!(
            frame.method() == "session.start",
            "personal RC authorization requires a start"
        );
        let renewal = renewal.context("personal RC start requires authenticated verification")?;
        let params = frame.params.as_ref().context("missing start parameters")?;
        let start: SessionStart = serde_json::from_value(params.clone())
            .map_err(|_| anyhow::anyhow!("invalid personal RC start parameters"))?;
        ensure!(
            start.workspace_id == crate::rc::endpoint::WORKSPACE && start.origin.is_none(),
            "personal RC start is outside its workspace"
        );
        let scope = StartScope {
            authorization_id: id.clone(),
            project_id: start.project_id,
            runtime: start.runtime,
            start_id: uuid::Uuid::parse_str(
                start.start_id.as_deref().context("missing start ID")?,
            )?
            .to_string(),
            permission_mode: serde_json::to_value(
                start.permission_mode.context("missing start mode")?,
            )?
            .as_str()
            .context("invalid start mode")?
            .into(),
        };
        let mut current = grant.clone();
        current.expires_at_ms = lease.expires_at_ms();
        let verified = renewal
            .api
            .verify_personal_rc_start(&renewal.credential, &renewal.token, &current, &scope)
            .await?;
        ensure!(
            *live
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            "personal RC authorization was refused"
        );
        let remaining = verified
            .expires_at_ms
            .saturating_sub(chrono::Utc::now().timestamp_millis())
            .max(0);
        let mut scope = verified.scope;
        scope.start_id = launch_key(&grant.caller, &scope.start_id);
        Ok::<_, anyhow::Error>(Proof {
            scope,
            caller: serde_json::json!([grant.caller.issuer, grant.caller.account_id]).to_string(),
            project_path: verified.project_path.into(),
            live: live.clone(),
            expires_at_ms: verified.expires_at_ms,
            deadline: tokio::time::Instant::now()
                + std::time::Duration::from_millis(remaining as u64),
        })
    }
    .await;
    match result {
        Ok(proof) => frame.authority.personal_rc_start = Some(proof),
        Err(error) => {
            // A refusal invalidates this controller's queued requests across its connections.
            // Another controller copying the public record ID cannot cancel them.
            *live
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = false;
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agit_peer::{
        Identity,
        access::{Access, Policy, Principal, Resource, Rule},
        cloud::{Device, DeviceCredential, Secret},
    };
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    /// A copied ID cannot authorize a launch; a verified launch keeps the operator ceiling and
    /// loses queued admission when a later Hub observation refuses the same record.
    #[tokio::test]
    async fn verified_start_is_exact_nonwire_and_revokes_already_queued_requests() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let principal = Principal {
            issuer: origin.clone(),
            account_id: "owner".into(),
        };
        let identity = Identity::generate().unwrap();
        let device = |id: &str| Device {
            id: id.into(),
            machine_id: id.into(),
            display_name: id.into(),
            owner: principal.clone(),
            certificate: identity.certificate().clone(),
            credential_epoch: 1,
        };
        let expires = chrono::Utc::now().timestamp_millis() + 60_000;
        let grant = ConnectionGrant {
            id: "connection".into(),
            caller: principal.clone(),
            source: device("controller"),
            target: device("executor"),
            expires_at_ms: expires,
            session_controller: None,
            project_controller: None,
        };
        let registry = super::super::ingress::Registry::fixed(
            Policy::new(
                1,
                vec![Rule {
                    principal: principal.clone(),
                    resource: Resource::Machine,
                    access: Access::Admin,
                }],
            )
            .unwrap(),
        );
        let client = registry.admitted(grant.clone()).await.unwrap();
        let catalog = client
            .authorize(Frame::request("workspace.list", json!({})))
            .unwrap();
        client.observe_response(&Frame::response(catalog.id.unwrap(), json!({"workspaces":[{
            "workspace_id":"local-owner", "projects":[{"project_id":"project", "local_path":"/project"}]
        }]})));
        let scope = StartScope {
            authorization_id: uuid::Uuid::new_v4().to_string(),
            project_id: "project".into(),
            runtime: "codex".into(),
            start_id: uuid::Uuid::new_v4().to_string(),
            permission_mode: "bypass".into(),
        };
        let response = json!({
            "scope": scope, "grant_id":grant.id, "caller":principal,
            "source_id":"controller", "target_id":"executor", "workspace_id":"hub-workspace",
            "project_path":"/project",
            "bot_id":"bot", "token_id":"token", "sender_id":"sender", "expires_at_ms":expires,
        });
        let expected = scope.clone();
        let server = tokio::spawn(async move {
            for status in [200, 403] {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                assert!(line.starts_with("POST /api/peer/personal-rc/verify-start "));
                let mut length = 0;
                loop {
                    line.clear();
                    socket.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                let mut body = vec![0; length];
                socket.read_exact(&mut body).await.unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["scope"], serde_json::to_value(&expected).unwrap());
                let body = if status == 200 {
                    response.to_string()
                } else {
                    "{}".into()
                };
                socket.write_all(format!("HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let renewal = Renewal {
            api: agit_peer::client::Client::new(&origin).unwrap(),
            credential: DeviceCredential {
                device: grant.target.clone(),
                token: Secret::new("test-device".into()),
            },
            token: Secret::new("test-connection".into()),
        };
        let request = || {
            Frame::request(
                "session.start",
                json!({
                    "workspace_id":"local-owner", "project_id":"project", "runtime":"codex",
                    "start_id":scope.start_id, "permission_mode":"bypass", "access_ceiling":"control",
                    "personal_rc_authorization":scope.authorization_id,
                }),
            )
        };
        let forged = client.authorize(request()).unwrap();
        assert!(!forged.authority.permits_personal_rc_start(
            &forged.params_as().unwrap(),
            forged.caller.as_ref().unwrap()
        ));
        let mut verified = request();
        verify(&mut verified, Some(&renewal), &grant, &client.lease())
            .await
            .unwrap();
        let admitted = client.authorize(verified).unwrap();
        let caller = admitted.caller.as_ref().unwrap();
        let start: SessionStart = admitted.params_as().unwrap();
        assert_eq!(caller.role, "operator");
        assert_ne!(start.start_id.as_deref(), Some(scope.start_id.as_str()));
        assert!(admitted.authority.permits_personal_rc_start(&start, caller));
        assert!(admitted.authority.owned_machine_grant().is_err());
        assert!(
            admitted
                .authority
                .check_project("project", std::path::Path::new("/project"))
                .is_ok()
        );
        assert!(
            admitted
                .authority
                .check_project("project", std::path::Path::new("/rebound"))
                .is_err()
        );
        assert!(
            admitted
                .authority
                .check_project("other", std::path::Path::new("/project"))
                .is_err()
        );
        for field in ["start_id", "project_id", "runtime", "permission_mode"] {
            let mut changed = serde_json::to_value(&start).unwrap();
            changed[field] = json!(match field {
                "start_id" => "00000000-0000-4000-8000-000000000000",
                "permission_mode" => "auto",
                _ => "other",
            });
            assert!(
                !admitted
                    .authority
                    .permits_personal_rc_start(&serde_json::from_value(changed).unwrap(), caller)
            );
        }
        let copied: Frame = serde_json::from_str(&admitted.to_json()).unwrap();
        assert!(!copied.authority.permits_personal_rc_start(&start, caller));
        let mut expired = admitted.authority.clone();
        expired.personal_rc_start.as_mut().unwrap().deadline = tokio::time::Instant::now();
        let (expired, receipt) = crate::rc::ticket::ticket_authorized::<()>(expired);
        assert!(!expired.accept());
        assert_eq!(receipt.abandon(), crate::rc::ticket::Abandon::NeverRan);
        let mut other = grant.clone();
        other.source.id = "another-controller".into();
        *observation(&other, &scope.authorization_id)
            .write()
            .unwrap() = false;
        assert!(admitted.authority.check().is_ok());
        let (queued, receipt) =
            crate::rc::ticket::ticket_authorized::<()>(admitted.authority.clone());
        assert!(
            verify(&mut request(), Some(&renewal), &grant, &client.lease())
                .await
                .is_err()
        );
        assert!(!queued.accept());
        assert_eq!(receipt.abandon(), crate::rc::ticket::Abandon::NeverRan);
        server.await.unwrap();
    }
}
