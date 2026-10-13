//! Session policy operations require registration, without selecting an accepted envelope.

use crate::domain::session_grants::{
    GrantPolicyMutation, GrantPolicyResult, GrantTarget, SessionGrantPolicy,
};
use crate::hub::{Client, identity::RemoteIdentity};
use anyhow::Result;

impl Client {
    pub fn original_grant_policy(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        session: &str,
    ) -> Result<SessionGrantPolicy> {
        let target = GrantTarget::session(&identity.agent_id, session)?;
        let response: SessionGrantPolicy =
            self.with_repository_route(repository, identity, |repository| {
                let path =
                    self.grant_path(repository, identity, &target, "original-grant-policy")?;
                self.get_expected(&path, Some(&identity.agent_id))
            })?;
        response.verify(&identity.agent_id, session)?;
        Ok(response)
    }

    pub fn change_original_grant_policy(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        request: &GrantPolicyMutation,
    ) -> Result<GrantPolicyResult> {
        request.validate()?;
        let response: GrantPolicyResult = self
            .with_repository_route(repository, identity, |repository| {
                let path = self.grant_path(
                    repository,
                    identity,
                    &request.target,
                    "original-grant-policy",
                )?;
                self.post_expected(&path, request, Some(&identity.agent_id))
            })
            .map_err(super::grants::access_error)?;
        response.verify(request)?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::session_grants::{GrantMutation, GrantOperation, GrantScope};
    use crate::hub::{client::ApiError, privacy::test_server::server_with_headers};
    use serde_json::{Value, json};

    /// Retries retain the confirmed scope, and an idempotent disable need not advance revisions.
    #[test]
    fn session_policy_and_grants_bind_retries_before_any_publication() {
        let target = GrantTarget::session(
            "00000000-0000-0000-0000-000000000001",
            &format!("agit-{}", "a".repeat(40)),
        )
        .unwrap();
        let enabled = SessionGrantPolicy {
            agent_id: target.agent_id().into(),
            session_id: target.session_id().into(),
            grant_scope: GrantScope::Session,
            policy_revision: 1,
            grant_revision: 1,
        };
        let disabled = SessionGrantPolicy {
            grant_scope: GrantScope::Commit,
            policy_revision: 2,
            grant_revision: 3,
            ..enabled.clone()
        };
        let conversion =
            json!({"policy":disabled,"converted_recipients":1,"converted_snapshots":0});
        let mut wrong_policy = json!(enabled);
        wrong_policy["session_id"] = json!(format!("agit-{}", "b".repeat(40)));
        let (anonymous, identity, worker) = server_with_headers(
            |_| {
                vec![
                    (200, json!(enabled)),
                    (
                        200,
                        json!({"target":target,"policy_revision":1,"grant_revision":1,"readers":[{
                "account_id":"bob","username":"bob","current_member":true,"owner_default":false,
                "committer_default":false,"explicit_grant":false,"session_explicit_grant":true,
                "revoked":false,"session_revoked":false,"default_suppressed":false,
                "membership_invalidated":false,"session_membership_invalidated":false,"can_read":true}]}),
                    ),
                    (
                        200,
                        json!({"id":"frozen-members","target":target,"policy_revision":1,"grant_revision":1,
                "members":[{"account_id":"bob","username":"bob"}],"expires_at":"2999-01-01T00:00:00Z"}),
                    ),
                    (
                        503,
                        json!({"kind":"response_unavailable","error":"reply lost"}),
                    ),
                    (
                        200,
                        json!({"target":target,"policy_revision":1,"grant_revision":2}),
                    ),
                    (200, conversion.clone()),
                    (
                        409,
                        json!({"kind":"conflict","error":"grant policy changed"}),
                    ),
                    (200, conversion.clone()),
                    (200, conversion),
                    (
                        400,
                        json!({"kind":"unsupported","error":"Enabling session-level authorization after disabling it is unsupported."}),
                    ),
                    (200, wrong_policy),
                ]
            },
            "Cache-Control: private, no-store\r\n",
        );
        let client = Client::for_hub_with_token(anonymous.base(), "synthetic-account-token");
        let mut policy = client
            .original_grant_policy("team/notes", &identity, target.session_id())
            .unwrap();
        let state = client
            .original_grants("team/notes", &identity, &target, policy.policy_revision)
            .unwrap();
        assert!(state.readers[0].can_read);
        let preview = client
            .preview_original_grants("team/notes", &identity, &target, policy.policy_revision)
            .unwrap();
        let grant = preview.mutation(&target, policy.policy_revision).unwrap();
        assert_eq!(preview.members[0].account_id, "bob");
        assert!(
            client
                .mutate_original_grants("team/notes", &identity, &grant)
                .is_err()
        );
        policy.grant_revision = client
            .mutate_original_grants("team/notes", &identity, &grant)
            .unwrap()
            .grant_revision;
        let disable = GrantPolicyMutation::disable(&policy).unwrap();
        let result = client
            .change_original_grant_policy("team/notes", &identity, &disable)
            .unwrap();
        let stale = client
            .mutate_original_grants("team/notes", &identity, &grant)
            .unwrap_err();
        assert_eq!(stale.downcast_ref::<ApiError>().unwrap().status, 409);
        client
            .change_original_grant_policy("team/notes", &identity, &disable)
            .unwrap();
        let mut repeated = GrantPolicyMutation::disable(&result.policy).unwrap();
        let repeated_result = client
            .change_original_grant_policy("team/notes", &identity, &repeated)
            .unwrap();
        assert_eq!(repeated_result.policy, result.policy);
        repeated.grant_scope = GrantScope::Session;
        let unsupported = client
            .change_original_grant_policy("team/notes", &identity, &repeated)
            .unwrap_err();
        assert_eq!(
            unsupported.downcast_ref::<ApiError>().unwrap().kind,
            "unsupported"
        );
        assert!(
            unsupported
                .to_string()
                .starts_with("Enabling session-level authorization")
        );
        assert!(!unsupported.to_string().contains("retry"));
        assert!(
            client
                .original_grant_policy("team/notes", &identity, target.session_id())
                .is_err()
        );
        let requests = worker.join().unwrap();
        let body = |index: usize| -> Value {
            serde_json::from_str(requests[index].split_once("\r\n\r\n").unwrap().1).unwrap()
        };
        assert_eq!(body(3), body(4));
        assert_eq!(body(3), body(6));
        assert_eq!(body(5), body(7));
        assert_eq!(body(3)["target"]["scope"], "session");
        assert!(requests[1].contains("scope=session&expected_policy_revision=1"));
        for request in requests {
            assert!(!request.contains("envelope_digest"));
            assert!(!request.contains("published_commit_id"));
            assert!(
                request
                    .to_lowercase()
                    .contains("x-agentgit-expected-agent-id:")
            );
        }
        let invalid = GrantMutation::new(target, 0, 1, GrantOperation::StopExplicit);
        assert!(invalid.validate().is_err());
    }
}
