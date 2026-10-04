//! Endpoint admission grants only the resource authority needed by one request.

use super::resources::{Resources, Session};
use crate::protocol::{CallerClaim, ErrorCode, Frame, RpcError};
use agit_peer::access::{Access, Policy, Principal};

#[derive(Clone, Copy)]
enum Need {
    Read,
    Control,
    Admin,
}

#[derive(Clone)]
enum Target {
    Catalog,
    Machine,
    Project(String),
    Session(Session),
}

#[derive(Clone)]
pub struct Permit {
    pub method: String,
    target: Target,
    need: Need,
    ceiling: Access,
    start_id: Option<String>,
    resolve_session: Option<String>,
}

fn denied() -> RpcError {
    RpcError::new(
        ErrorCode::Forbidden,
        "this cloud account is not permitted to access the executor resource",
    )
}

impl Permit {
    pub fn authority_matches(
        &self,
        resources: &Resources,
        policy: &Policy,
        principal: &Principal,
        role: &str,
    ) -> bool {
        let mut current = self.clone();
        if let Target::Session(session) = &self.target {
            let Some(session) = resources.session(&session.id) else {
                return false;
            };
            current.target = Target::Session(session.clone());
        }
        current.allowed(policy, principal) && current.access(policy, principal).role() == role
    }

    pub fn observe(&self, resources: &mut Resources, result: &serde_json::Value) {
        if self.method == "session.resume"
            && let Target::Session(session) = &self.target
        {
            resources.resumed(session, &result["session"]);
        }
        resources.observe(&self.method, result);
    }

    fn access(&self, policy: &Policy, principal: &Principal) -> Access {
        let access = match &self.target {
            Target::Catalog => {
                if policy.access(principal, None, None).is_admin() {
                    Access::Admin
                } else {
                    Access::Read
                }
            }
            Target::Machine => policy.access(principal, None, None),
            Target::Project(project) => policy.access(principal, None, Some(project)),
            Target::Session(session) => session.access(policy, principal),
        };
        // A controller can narrow its own authority, never widen executor policy.
        match (access, self.ceiling) {
            (Access::Deny, _) | (_, Access::Deny) => Access::Deny,
            (Access::Read, _) | (_, Access::Read) => Access::Read,
            (Access::Control, _) | (_, Access::Control) => Access::Control,
            _ => Access::Admin,
        }
    }

    pub fn allowed(&self, policy: &Policy, principal: &Principal) -> bool {
        let access = self.access(policy, principal);
        match self.need {
            Need::Read => access.can_read(),
            Need::Control => access.can_control(),
            Need::Admin => access.is_admin(),
        }
    }

    pub fn response(
        &self,
        mut frame: Frame,
        resources: &Resources,
        policy: &Policy,
        principal: &Principal,
    ) -> Frame {
        let mut current = self.clone();
        if let Target::Session(session) = &self.target {
            let Some(session) = resources.session(&session.id) else {
                return Frame::error_response(frame.id.clone().unwrap(), denied());
            };
            current.target = Target::Session(session.clone());
        }
        if !current.allowed(policy, principal) {
            return Frame::error_response(
                frame
                    .id
                    .clone()
                    .unwrap_or(crate::protocol::RequestId::Num(0)),
                denied(),
            );
        }
        if let Some(result) = &mut frame.result {
            if let Some(id) = result["session"]["session_id"].as_str()
                && !resources
                    .session(id)
                    .is_some_and(|session| session.access(policy, principal).can_read())
            {
                return Frame::error_response(frame.id.clone().unwrap(), denied());
            }
            resources.filter(&self.method, result, policy, principal);
            if let Some(id) = &self.resolve_session {
                result["resolved_session"] = resources
                    .session(id)
                    .filter(|session| {
                        !session.native_id.is_empty()
                            && session.access(policy, principal).can_read()
                    })
                    .map_or(serde_json::Value::Null, |session| {
                        let mut identity = serde_json::json!({
                            "session_id":session.id,
                            "runtime_session_id":session.native_id,
                            "runtime":session.runtime,
                            "project_id":session.project,
                            "workspace_id":super::super::endpoint::WORKSPACE,
                        });
                        if let Some(source) = &session.source {
                            identity["native_source"] = serde_json::json!(source);
                        }
                        identity
                    });
            }
            if let Some(start_id) = &self.start_id {
                result["start_id"] = serde_json::json!(start_id);
            }
        }
        frame
    }
}

pub fn authorize(
    mut frame: Frame,
    principal: &Principal,
    policy: &Policy,
    resources: &mut Resources,
) -> Result<(Frame, Permit), RpcError> {
    super::super::endpoint::validate_request(&frame)?;
    let method = frame.method().to_owned();
    let params = frame.params.get_or_insert_with(|| serde_json::json!({}));
    let resolve_session = (method == "session.list")
        .then(|| params["resolve_session"].as_str().map(str::to_owned))
        .flatten();
    let ceiling = params
        .as_object_mut()
        .and_then(|params| params.remove("access_ceiling"))
        .map(serde_json::from_value::<Access>)
        .transpose()
        .map_err(|_| RpcError::new(ErrorCode::MalformedFrame, "invalid cloud access ceiling"))?
        .unwrap_or(Access::Admin);
    let mut start_id = None;
    let selection = match method.as_str() {
        "machine.describe" | "workspace.list" | "session.list" | "session.catalog.list" => {
            (Target::Catalog, Need::Read)
        }
        "session.catalog.settings"
        | "session.history"
        | "session.goal.read"
        | "session.publication.deliver"
        | "session.subscribe"
        | "session.watch"
        | "session.unwatch"
        | "session.commands"
        | "session.model"
        | "session.permissions"
        | "session.approvals"
        | "session.resume"
        | "session.enqueue"
        | "turn.start"
        | "turn.steer"
        | "turn.interrupt"
        | "approval.decide"
        | "session.command"
        | "session.setModel"
        | "session.setPermissionMode" => {
            let id = params["session_id"].as_str().ok_or_else(denied)?.to_owned();
            let resolved = if matches!(
                method.as_str(),
                "session.catalog.settings"
                    | "session.publication.deliver"
                    | "session.goal.read"
                    | "session.resume"
                    | "session.watch"
                    | "session.unwatch"
                    | "session.history"
                    | "session.enqueue"
            ) {
                resources.resolve_catalog(&id).map_err(|_| {
                    RpcError::new(
                        ErrorCode::SessionNotFound,
                        "conversation source is unavailable; refresh the catalog",
                    )
                })?
            } else {
                resources.session(&id).cloned()
            };
            let session = resolved.ok_or_else(|| {
                RpcError::new(
                    ErrorCode::SessionNotFound,
                    "refresh the executor session catalog before addressing this session",
                )
            })?;
            if let Some(source) = &session.source {
                let managed_operation = session.id.starts_with("agit-")
                    && matches!(
                        method.as_str(),
                        "session.history"
                            | "session.subscribe"
                            | "session.model"
                            | "session.permissions"
                            | "session.approvals"
                            | "session.commands"
                            | "session.command"
                            | "session.enqueue"
                            | "turn.start"
                            | "turn.steer"
                            | "session.setModel"
                            | "session.setPermissionMode"
                            | "approval.decide"
                            | "turn.interrupt"
                    );
                let watch_read =
                    id == super::super::daemon::watch_stream_id(
                        super::super::endpoint::WORKSPACE,
                        &source.session_ref(&session.native_id),
                    ) && method == "session.subscribe";
                if !matches!(
                    method.as_str(),
                    "session.catalog.settings"
                        | "session.publication.deliver"
                        | "session.goal.read"
                        | "session.resume"
                        | "session.watch"
                        | "session.unwatch"
                        | "session.history"
                        | "session.enqueue"
                ) && !managed_operation
                    && !watch_read
                {
                    return Err(RpcError::new(
                        ErrorCode::SessionNotFound,
                        "this operation requires source-aware session attachment",
                    ));
                }
                if method == "session.publication.deliver" {
                    for field in [
                        "source_id",
                        "source_generation",
                        "native_session_id",
                        "expected_cwd",
                    ] {
                        params
                            .as_object_mut()
                            .expect("request parameters")
                            .remove(field);
                    }
                } else {
                    params["source_id"] = serde_json::json!(source.source_id);
                    params["source_generation"] = serde_json::json!(source.generation);
                    params["native_session_id"] = serde_json::json!(session.native_id);
                    params["expected_cwd"] = serde_json::json!(session.cwd);
                }
            } else if method == "session.catalog.settings" {
                return Err(RpcError::new(
                    ErrorCode::SessionNotFound,
                    "refresh the source-qualified session catalog",
                ));
            }
            let need = if method == "session.command" {
                Need::Admin
            } else if matches!(
                method.as_str(),
                "session.history"
                    | "session.goal.read"
                    | "session.subscribe"
                    | "session.watch"
                    | "session.unwatch"
                    | "session.commands"
                    | "session.model"
                    | "session.permissions"
                    | "session.approvals"
                    | "session.catalog.settings"
            ) {
                Need::Read
            } else {
                Need::Control
            };
            if matches!(method.as_str(), "session.history" | "session.goal.read") {
                params["cwd"] = serde_json::json!(session.cwd);
                params["runtime"] = serde_json::json!(session.runtime);
            }
            if method == "session.watch" {
                resources.watch_alias(&id);
            }
            (Target::Session(session), need)
        }
        "session.start" => {
            let project = params["project_id"].as_str().ok_or_else(denied)?.to_owned();
            if !resources.projects.contains_key(&project) {
                return Err(denied());
            }
            if let Some(id) = params["start_id"].as_str() {
                let id = uuid::Uuid::parse_str(id)
                    .map_err(|_| {
                        RpcError::new(
                            ErrorCode::MalformedFrame,
                            "session.start start_id must be a UUID",
                        )
                    })?
                    .to_string();
                params["start_id"] = serde_json::json!(launch_key(principal, &id));
                start_id = Some(id);
            }
            (Target::Project(project), Need::Control)
        }
        "runtime.models" => {
            if let Some(project) = params["project_id"].as_str().map(str::to_owned) {
                let path = resources.projects.get(&project).ok_or_else(denied)?;
                params["cwd"] = serde_json::json!(path);
                (Target::Project(project), Need::Read)
            } else {
                (Target::Machine, Need::Admin)
            }
        }
        "fs.readDirectory"
        | "fs.readFile"
        | "project.bind"
        | "project.publication.bind"
        | "project.unbind"
        | "terminal.open"
        | "terminal.input"
        | "terminal.resize"
        | "terminal.close" => (Target::Machine, Need::Admin),
        _ => return Err(denied()),
    };
    let permit = Permit {
        method,
        target: selection.0,
        need: selection.1,
        ceiling,
        start_id,
        resolve_session,
    };
    if !permit.allowed(policy, principal) {
        return Err(denied());
    }
    frame.caller = Some(CallerClaim {
        account_id: Some(serde_json::json!([principal.issuer, principal.account_id]).to_string()),
        username: None,
        role: permit.access(policy, principal).role().into(),
        workspace_id: super::super::endpoint::WORKSPACE.into(),
    });
    Ok((frame, permit))
}

pub(super) fn launch_key(principal: &Principal, id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(
        serde_json::json!(["cloud-launch", principal, id])
            .to_string()
            .as_bytes(),
    );
    let mut bytes: [u8; 16] = digest[..16].try_into().unwrap();
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

pub fn event_allowed(
    frame: &Frame,
    resources: &Resources,
    policy: &Policy,
    principal: &Principal,
) -> bool {
    if matches!(frame.method(), "terminal.output" | "terminal.exited") {
        return policy.access(principal, None, None).is_admin();
    }
    frame
        .stream
        .as_deref()
        .and_then(|stream| resources.session(stream))
        .is_some_and(|session| session.access(policy, principal).can_read())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agit_peer::access::{Resource, Rule};
    use serde_json::json;

    #[test]
    fn managed_source_operations_preserve_executor_identity_and_access() {
        let principal = Principal {
            issuer: "https://cloud.example".into(),
            account_id: "operator".into(),
        };
        let source = crate::protocol::NativeSourceRef {
            source_id: "source-a".into(),
            generation: 3,
        };
        let reference = source.session_ref("native");
        let policy = |access| {
            Policy::new(
                1,
                vec![Rule {
                    principal: principal.clone(),
                    resource: Resource::Session(reference.clone()),
                    access,
                }],
            )
            .unwrap()
        };
        let mut resources = Resources::default();
        resources.observe(
            "workspace.list",
            &json!({"workspaces":[{"workspace_id":"local-owner", "projects":[{
                "project_id":"project", "local_path":"/project"
            }]}]}),
        );
        resources.observe(
            "session.catalog.list",
            &json!({"rows":[{
                "session_ref":reference, "source_id":"source-a", "source_generation":3,
                "native_session_id":"native", "runtime":"codex", "cwd":"/project"
            }]}),
        );
        assert!(resources.session(&reference).is_some());
        resources.observe(
            "session.start",
            &json!({"session": {
                "session_id":"agit-managed", "runtime_session_id":"native",
                "workspace_id":"local-owner", "project_id":"project", "runtime":"codex",
                "native_source":{"source_id":"source-a", "generation":3}
            }}),
        );
        let delivery = Frame::request(
            "session.publication.deliver",
            json!({
                "session_id":reference, "source_id":"forged", "native_session_id":"foreign"
            }),
        );
        let (delivery, permit) = authorize(
            delivery.clone(),
            &principal,
            &policy(Access::Control),
            &mut resources,
        )
        .unwrap();
        let params = delivery.params.as_ref().unwrap();
        assert_eq!(params["session_id"], reference);
        assert!(params.get("source_id").is_none());
        assert!(params.get("native_session_id").is_none());
        assert!(!permit.authority_matches(
            &resources,
            &policy(Access::Read),
            &principal,
            "operator"
        ));
        assert!(authorize(delivery, &principal, &policy(Access::Read), &mut resources).is_err());
        for method in ["session.permissions", "session.approvals"] {
            let (metadata, permit) = authorize(
                Frame::request(method, json!({"session_id":"agit-managed"})),
                &principal,
                &policy(Access::Read),
                &mut resources,
            )
            .unwrap();
            assert_eq!(
                metadata.params.as_ref().unwrap()["session_id"],
                "agit-managed"
            );
            assert!(permit.authority_matches(
                &resources,
                &policy(Access::Read),
                &principal,
                "viewer"
            ));
            assert!(!permit.authority_matches(
                &resources,
                &Policy::default(),
                &principal,
                "viewer"
            ));
        }
        for method in ["turn.start", "turn.steer"] {
            let request = Frame::request(
                method,
                json!({
                    "session_id":"agit-managed", "message":"Continue the task",
                    "source_id":"forged", "source_generation":1,
                    "native_session_id":"foreign", "expected_cwd":"/private"
                }),
            );
            let (admitted, permit) = authorize(
                request.clone(),
                &principal,
                &policy(Access::Control),
                &mut resources,
            )
            .unwrap();
            let params = admitted.params.unwrap();
            assert_eq!(params["session_id"], "agit-managed");
            assert_eq!(params["source_id"], "source-a");
            assert_eq!(params["source_generation"], 3);
            assert_eq!(params["native_session_id"], "native");
            assert_eq!(params["expected_cwd"], "/project");
            assert!(!permit.authority_matches(
                &resources,
                &policy(Access::Read),
                &principal,
                "operator"
            ));
            assert!(authorize(request, &principal, &policy(Access::Read), &mut resources).is_err());
            let unattached = Frame::request(
                method,
                json!({
                    "session_id":reference, "message":"Continue the task"
                }),
            );
            assert!(
                authorize(
                    unattached,
                    &principal,
                    &policy(Access::Control),
                    &mut resources
                )
                .is_err()
            );
        }
        let command = Frame::request(
            "session.command",
            json!({
                "session_id":"agit-managed", "name":"compact", "arguments":{},
                "source_id":"forged", "source_generation":1,
                "native_session_id":"foreign", "expected_cwd":"/private"
            }),
        );
        let (admitted, permit) = authorize(
            command.clone(),
            &principal,
            &policy(Access::Admin),
            &mut resources,
        )
        .unwrap();
        let params = admitted.params.unwrap();
        assert_eq!(params["session_id"], "agit-managed");
        assert_eq!(params["source_id"], "source-a");
        assert_eq!(params["source_generation"], 3);
        assert_eq!(params["native_session_id"], "native");
        assert_eq!(params["expected_cwd"], "/project");
        assert!(!permit.authority_matches(
            &resources,
            &policy(Access::Control),
            &principal,
            "operator"
        ));
        assert!(
            authorize(
                command,
                &principal,
                &policy(Access::Control),
                &mut resources
            )
            .is_err()
        );
        assert!(
            authorize(
                Frame::request(
                    "session.command",
                    json!({"session_id":reference, "name":"compact"})
                ),
                &principal,
                &policy(Access::Admin),
                &mut resources,
            )
            .is_err()
        );
    }

    #[test]
    fn saved_identity_resolution_uses_executor_records_and_current_read_authority() {
        let principal = Principal {
            issuer: "https://cloud.example".into(),
            account_id: "operator".into(),
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
            .insert("project".into(), "/trusted".into());
        resources.observe(
            "session.list",
            &json!({"sessions":[{
                "session_id":"saved", "runtime_session_id":"native", "runtime":"codex",
                "project_id":"project", "workspace_id":"local-owner",
            }]}),
        );
        let (request, permit) = authorize(
            Frame::request(
                "session.list",
                json!({
                    "resolve_session":"saved", "runtime_session_id":"untrusted",
                }),
            ),
            &principal,
            &policy,
            &mut resources,
        )
        .unwrap();
        let response = Frame::response(
            request.id.clone().unwrap(),
            json!({"sessions":[], "local":[]}),
        );
        let result = permit
            .response(response.clone(), &resources, &policy, &principal)
            .result
            .unwrap();
        assert_eq!(result["sessions"], json!([]));
        assert_eq!(
            result["resolved_session"],
            json!({
                "session_id":"saved", "runtime_session_id":"native", "runtime":"codex",
                "project_id":"project", "workspace_id":"local-owner",
            })
        );
        let revoked = permit.response(response, &resources, &Policy::default(), &principal);
        assert!(revoked.result.unwrap()["resolved_session"].is_null());

        let source = crate::protocol::NativeSourceRef {
            source_id: "enrolled-home".into(),
            generation: 3,
        };
        resources.observe(
            "session.list",
            &json!({"sessions":[{
                "session_id":"saved", "runtime_session_id":"native", "runtime":"codex",
                "project_id":"project", "workspace_id":"local-owner", "native_source":source,
            }]}),
        );
        let response = Frame::response(request.id.unwrap(), json!({"sessions":[], "local":[]}));
        let result = permit
            .response(response.clone(), &resources, &policy, &principal)
            .result
            .unwrap();
        assert_eq!(result["resolved_session"]["native_source"], json!(source));
        assert_eq!(result["resolved_session"]["runtime_session_id"], "native");
        assert!(result["sessions"].as_array().unwrap().is_empty());
        let revoked = permit.response(response, &resources, &Policy::default(), &principal);
        assert!(revoked.result.unwrap()["resolved_session"].is_null());
    }

    #[test]
    fn native_inbox_requires_control_of_the_exact_session() {
        let principal = Principal {
            issuer: "https://cloud.example".into(),
            account_id: "operator".into(),
        };
        let mut resources = Resources::default();
        resources.observe(
            "session.list",
            &json!({"local":[{
                "runtime_session_id":"native", "runtime":"codex", "cwd":"/trusted"
            }]}),
        );
        for (access, allowed) in [(Access::Read, false), (Access::Control, true)] {
            let policy = Policy::new(
                1,
                vec![Rule {
                    principal: principal.clone(),
                    resource: Resource::Session("native".into()),
                    access,
                }],
            )
            .unwrap();
            let request = Frame::request("session.enqueue", json!({"session_id":"native"}));
            assert_eq!(
                authorize(request, &principal, &policy, &mut resources).is_ok(),
                allowed
            );
            let other = Frame::request("session.enqueue", json!({"session_id":"another"}));
            assert!(authorize(other, &principal, &policy, &mut resources).is_err());
        }
    }

    #[test]
    fn launch_receipts_are_principal_scoped_and_echo_the_client_intent() {
        let principal = Principal {
            issuer: "https://cloud.example".into(),
            account_id: "operator".into(),
        };
        let other = Principal {
            account_id: "another".into(),
            ..principal.clone()
        };
        let id = uuid::Uuid::new_v4().to_string();
        assert_ne!(launch_key(&principal, &id), launch_key(&other, &id));
        let policy = Policy::new(
            1,
            vec![Rule {
                principal: principal.clone(),
                resource: Resource::Project("project".into()),
                access: Access::Control,
            }],
        )
        .unwrap();
        let mut resources = Resources::default();
        resources
            .projects
            .insert("project".into(), "/trusted".into());
        let request = Frame::request(
            "session.start",
            json!({"project_id":"project","start_id":id}),
        );
        let (request, permit) = authorize(request, &principal, &policy, &mut resources).unwrap();
        let internal = request.params.unwrap()["start_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(internal, launch_key(&principal, &id));
        let result = json!({"start_id":internal,"session":{"session_id":"logical","workspace_id":"local-owner","project_id":"project","runtime":"codex"}});
        permit.observe(&mut resources, &result);
        let response = permit.response(
            Frame::response(request.id.unwrap(), result),
            &resources,
            &policy,
            &principal,
        );
        assert_eq!(response.result.unwrap()["start_id"], id);
    }

    #[test]
    fn resumed_aliases_keep_native_access_and_honor_a_new_logical_denial() {
        let principal = Principal {
            issuer: "https://cloud.example".into(),
            account_id: "operator".into(),
        };
        let rule = |resource, access| Rule {
            principal: principal.clone(),
            resource,
            access,
        };
        let native = rule(Resource::Session("native".into()), Access::Control);
        let policy = Policy::new(1, vec![native.clone()]).unwrap();
        let mut resources = Resources::default();
        resources.observe(
            "session.list",
            &json!({"local":[{"runtime_session_id":"native","runtime":"codex","cwd":"/trusted"}]}),
        );
        resources.watch_alias("native");
        let request = Frame::request("session.resume", json!({"session_id":"native"}));
        let (request, permit) = authorize(request, &principal, &policy, &mut resources).unwrap();
        let result = json!({"session":{"session_id":"logical","runtime":"codex","workspace_id":"local-owner"}});
        permit.observe(&mut resources, &result);
        let request = Frame::response(request.id.unwrap(), result);
        assert!(
            permit
                .response(request.clone(), &resources, &policy, &principal)
                .error
                .is_none()
        );
        let denied = Policy::new(
            2,
            vec![
                native,
                rule(Resource::Session("logical".into()), Access::Deny),
            ],
        )
        .unwrap();
        assert!(
            permit
                .response(request, &resources, &denied, &principal)
                .error
                .is_some()
        );
        for id in ["native", "logical", "agit-watch-local-owner-native"] {
            assert_eq!(
                resources.session(id).unwrap().access(&denied, &principal),
                Access::Deny
            );
        }
    }

    #[test]
    fn session_reads_use_executor_coordinates_and_revocation_filters_pending_responses() {
        let principal = Principal {
            issuer: "https://cloud.example".into(),
            account_id: "reader".into(),
        };
        let policy = Policy::new(
            1,
            vec![Rule {
                principal: principal.clone(),
                resource: Resource::Session("native".into()),
                access: Access::Read,
            }],
        )
        .unwrap();
        let mut resources = Resources::default();
        resources.observe("session.list", &json!({"local":[{"runtime_session_id":"native","runtime":"codex","cwd":"/trusted/project"}]}));
        let read = Frame::request(
            "session.history",
            json!({"session_id":"native","cwd":"/untrusted/path","runtime":"other"}),
        );
        let (read, permit) = authorize(read, &principal, &policy, &mut resources).unwrap();
        assert_eq!(read.params.as_ref().unwrap()["cwd"], "/trusted/project");
        assert_eq!(read.params.as_ref().unwrap()["runtime"], "codex");
        let write = Frame::request("turn.start", json!({"session_id":"native"}));
        assert!(authorize(write, &principal, &policy, &mut resources).is_err());
        let response = Frame::response(read.id.unwrap(), json!({"items":["private transcript"]}));
        let redacted = permit.response(response, &resources, &Policy::default(), &principal);
        assert!(redacted.error.is_some());
        assert!(redacted.result.is_none());
        let forged = Frame::request("peer.connect", json!({"peer_id":"other-machine"}));
        assert!(authorize(forged, &principal, &policy, &mut resources).is_err());
    }
}
