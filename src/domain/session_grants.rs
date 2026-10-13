//! Mutable original-read authorization is independent of immutable encryption identity.

use super::session_privacy::{
    AcceptedSnapshot, validate_identifier, validate_oid, validate_repository_id,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantScope {
    Session,
    Commit,
}

impl GrantScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Commit => "commit",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub enum GrantTarget {
    Session {
        agent_id: String,
        session_id: String,
    },
    Commit {
        agent_id: String,
        session_id: String,
        published_commit_id: String,
        envelope_digest: String,
    },
}

impl GrantTarget {
    pub fn session(agent_id: &str, session_id: &str) -> Result<Self> {
        let target = Self::Session {
            agent_id: agent_id.into(),
            session_id: session_id.into(),
        };
        target.validate()?;
        Ok(target)
    }

    pub fn from_snapshot(snapshot: &AcceptedSnapshot) -> Result<Self> {
        snapshot.validate()?;
        Ok(Self::Commit {
            agent_id: snapshot.binding.repository_id.clone(),
            session_id: snapshot.binding.session_id.clone(),
            published_commit_id: snapshot.published_commit.clone(),
            envelope_digest: snapshot.envelope_digest.clone(),
        })
    }

    pub fn agent_id(&self) -> &str {
        match self {
            Self::Session { agent_id, .. } | Self::Commit { agent_id, .. } => agent_id,
        }
    }

    pub fn session_id(&self) -> &str {
        match self {
            Self::Session { session_id, .. } | Self::Commit { session_id, .. } => session_id,
        }
    }

    pub fn scope(&self) -> GrantScope {
        match self {
            Self::Session { .. } => GrantScope::Session,
            Self::Commit { .. } => GrantScope::Commit,
        }
    }

    pub fn validate(&self) -> Result<()> {
        validate_repository_id(self.agent_id())?;
        ensure!(
            super::meta::is_bare_id(self.session_id()),
            "invalid logical session identity"
        );
        if let Self::Commit {
            published_commit_id,
            envelope_digest,
            ..
        } = self
        {
            validate_oid(published_commit_id)?;
            ensure!(
                envelope_digest.starts_with("sha256:") && envelope_digest.len() == 71,
                "invalid original grant envelope digest"
            );
            validate_oid(&envelope_digest[7..])?;
        }
        Ok(())
    }

    pub fn verify(&self, expected: &Self) -> Result<()> {
        self.validate()?;
        expected.validate()?;
        ensure!(
            self == expected,
            "original access response differs from the requested target"
        );
        Ok(())
    }
}

/// The grant revision orders session grants and conversion; commit targets have their own revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionGrantPolicy {
    pub agent_id: String,
    pub session_id: String,
    pub grant_scope: GrantScope,
    pub policy_revision: i64,
    pub grant_revision: i64,
}

impl SessionGrantPolicy {
    pub fn target(&self) -> Result<GrantTarget> {
        GrantTarget::session(&self.agent_id, &self.session_id)
    }

    pub fn validate(&self) -> Result<()> {
        self.target()?;
        validate_revision(self.policy_revision)?;
        validate_revision(self.grant_revision)
    }

    pub fn verify(&self, agent_id: &str, session_id: &str) -> Result<()> {
        self.validate()?;
        ensure!(
            self.agent_id == agent_id && self.session_id == session_id,
            "original grant policy belongs to another repository or session"
        );
        Ok(())
    }

    pub fn verify_target(&self, target: &GrantTarget) -> Result<()> {
        self.verify(target.agent_id(), target.session_id())?;
        target.validate()?;
        ensure!(
            self.grant_scope == target.scope(),
            "original grant scope changed; inspect access and confirm a new operation"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantPolicyMutation {
    pub target: GrantTarget,
    pub grant_scope: GrantScope,
    pub expected_policy_revision: i64,
    pub expected_grant_revision: i64,
    pub idempotency_key: String,
}

impl GrantPolicyMutation {
    pub fn disable(policy: &SessionGrantPolicy) -> Result<Self> {
        policy.validate()?;
        Ok(Self {
            target: policy.target()?,
            grant_scope: GrantScope::Commit,
            expected_policy_revision: policy.policy_revision,
            expected_grant_revision: policy.grant_revision,
            idempotency_key: uuid::Uuid::new_v4().to_string(),
        })
    }

    pub fn validate(&self) -> Result<()> {
        self.target.validate()?;
        ensure!(
            self.target.scope() == GrantScope::Session,
            "policy changes require a session target"
        );
        validate_revision(self.expected_policy_revision)?;
        validate_revision(self.expected_grant_revision)?;
        validate_identifier(&self.idempotency_key)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantPolicyResult {
    pub policy: SessionGrantPolicy,
    pub converted_recipients: u64,
    pub converted_snapshots: u64,
}

impl GrantPolicyResult {
    pub fn verify(&self, request: &GrantPolicyMutation) -> Result<()> {
        request.validate()?;
        self.policy
            .verify(request.target.agent_id(), request.target.session_id())?;
        ensure!(
            self.policy.grant_scope == request.grant_scope
                && self.policy.policy_revision >= request.expected_policy_revision
                && self.policy.grant_revision >= request.expected_grant_revision,
            "original grant policy response contradicts the requested change"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum GrantOperation {
    Grant { preview_id: String },
    Revoke { account_id: String },
    StopExplicit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantMutation {
    pub target: GrantTarget,
    pub expected_policy_revision: i64,
    pub expected_grant_revision: i64,
    pub idempotency_key: String,
    pub action: GrantOperation,
}

impl GrantMutation {
    pub fn new(
        target: GrantTarget,
        policy_revision: i64,
        grant_revision: i64,
        action: GrantOperation,
    ) -> Self {
        Self {
            target,
            expected_policy_revision: policy_revision,
            expected_grant_revision: grant_revision,
            idempotency_key: uuid::Uuid::new_v4().to_string(),
            action,
        }
    }

    pub fn validate(&self) -> Result<()> {
        self.target.validate()?;
        validate_revision(self.expected_policy_revision)?;
        validate_revision(self.expected_grant_revision)?;
        ensure!(
            self.expected_grant_revision < i64::MAX,
            "original grant revision is exhausted"
        );
        validate_identifier(&self.idempotency_key)?;
        match &self.action {
            GrantOperation::Grant { preview_id } => validate_identifier(preview_id)?,
            GrantOperation::Revoke { account_id } => validate_identifier(account_id)?,
            GrantOperation::StopExplicit => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantResult {
    pub target: GrantTarget,
    pub policy_revision: i64,
    pub grant_revision: i64,
}

impl GrantResult {
    pub fn verify(&self, request: &GrantMutation) -> Result<()> {
        request.validate()?;
        self.target.verify(&request.target)?;
        ensure!(
            self.policy_revision == request.expected_policy_revision
                && self.grant_revision == request.expected_grant_revision + 1,
            "original access mutation returned unexpected revisions"
        );
        Ok(())
    }
}

/// Each scope's membership epoch applies only to its own positive entitlements.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReaderAccess {
    pub current_member: bool,
    pub owner_default: bool,
    pub committer_default: bool,
    pub explicit_grant: bool,
    pub session_explicit_grant: bool,
    pub revoked: bool,
    pub session_revoked: bool,
    pub default_suppressed: bool,
    pub membership_invalidated: bool,
    pub session_membership_invalidated: bool,
}

impl ReaderAccess {
    pub fn can_read(&self) -> bool {
        self.current_member
            && !self.revoked
            && !self.session_revoked
            && ((!self.membership_invalidated
                && (self.explicit_grant
                    || (!self.default_suppressed
                        && (self.owner_default || self.committer_default))))
                || (!self.session_membership_invalidated && self.session_explicit_grant))
    }

    pub fn verify_scope(&self, scope: GrantScope) -> Result<()> {
        ensure!(
            scope == GrantScope::Session
                || (!self.session_explicit_grant
                    && !self.session_revoked
                    && !self.session_membership_invalidated
                    && !self.default_suppressed),
            "commit policy cannot carry active session entitlements"
        );
        Ok(())
    }
}

pub fn validate_revision(revision: i64) -> Result<()> {
    ensure!(revision > 0, "invalid original grant revision");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Session authorization never weakens snapshot identity or acquires an implicit commit target.
    #[test]
    fn scope_and_policy_are_explicit_and_independent_of_encryption() {
        let target = GrantTarget::session(
            "00000000-0000-0000-0000-000000000001",
            &format!("agit-{}", "a".repeat(40)),
        )
        .unwrap();
        let policy = SessionGrantPolicy {
            agent_id: target.agent_id().into(),
            session_id: target.session_id().into(),
            grant_scope: GrantScope::Session,
            policy_revision: 1,
            grant_revision: 1,
        };
        policy.verify_target(&target).unwrap();
        let mut value = serde_json::to_value(&target).unwrap();
        value["published_commit_id"] = json!("b".repeat(40));
        assert!(serde_json::from_value::<GrantTarget>(value).is_err());
        let request = GrantPolicyMutation::disable(&policy).unwrap();
        let mut result = GrantPolicyResult {
            policy: SessionGrantPolicy {
                grant_scope: GrantScope::Commit,
                policy_revision: 2,
                grant_revision: 2,
                ..policy
            },
            converted_recipients: 0,
            converted_snapshots: 0,
        };
        result.verify(&request).unwrap();
        let repeated = GrantPolicyMutation::disable(&result.policy).unwrap();
        result.verify(&repeated).unwrap();
        assert!(result.policy.verify_target(&target).is_err());
        result.policy.session_id = format!("agit-{}", "c".repeat(40));
        assert!(result.verify(&request).is_err());
    }

    /// Regrant can restore explicit access without reviving suppressed defaults or old memberships.
    #[test]
    fn effective_reader_preserves_revocation_and_scope_membership_boundaries() {
        let mut reader = ReaderAccess {
            current_member: true,
            owner_default: true,
            committer_default: false,
            explicit_grant: false,
            session_explicit_grant: true,
            revoked: false,
            session_revoked: false,
            default_suppressed: true,
            membership_invalidated: true,
            session_membership_invalidated: false,
        };
        assert!(reader.can_read());
        reader.session_revoked = true;
        assert!(!reader.can_read());
        reader.session_revoked = false;
        reader.revoked = true;
        assert!(!reader.can_read());
        reader.revoked = false;
        reader.session_membership_invalidated = true;
        assert!(!reader.can_read());
        reader.session_membership_invalidated = false;
        reader.session_explicit_grant = false;
        reader.membership_invalidated = false;
        assert!(!reader.can_read());
        reader.explicit_grant = true;
        assert!(reader.can_read());
        reader.current_member = false;
        assert!(!reader.can_read());
        assert!(reader.verify_scope(GrantScope::Commit).is_err());
        reader.default_suppressed = false;
        reader.owner_default = false;
        reader.verify_scope(GrantScope::Commit).unwrap();
        reader.session_explicit_grant = true;
        assert!(reader.verify_scope(GrantScope::Commit).is_err());
    }
}
