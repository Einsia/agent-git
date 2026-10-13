//! Original access changes bind an explicit scope and frozen policy and grant revisions.

pub use crate::domain::session_grants::{GrantMutation, GrantOperation, GrantResult, GrantTarget};
use crate::domain::{
    session_grants::{GrantScope, ReaderAccess, validate_revision},
    session_privacy::validate_identifier,
};
use crate::hub::{Client, client::ApiError, identity::RemoteIdentity};
use anyhow::{Result, ensure};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantMember {
    pub account_id: String,
    pub username: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantPreview {
    pub id: String,
    pub target: GrantTarget,
    pub policy_revision: i64,
    pub grant_revision: i64,
    pub members: Vec<GrantMember>,
    pub expires_at: String,
}

impl GrantPreview {
    pub fn mutation(&self, expected: &GrantTarget, policy_revision: i64) -> Result<GrantMutation> {
        self.validate(expected, policy_revision)?;
        Ok(GrantMutation::new(
            self.target.clone(),
            self.policy_revision,
            self.grant_revision,
            GrantOperation::Grant {
                preview_id: self.id.clone(),
            },
        ))
    }

    fn validate(&self, expected: &GrantTarget, policy_revision: i64) -> Result<()> {
        self.target.verify(expected)?;
        validate_revision(self.policy_revision)?;
        ensure!(
            self.policy_revision == policy_revision,
            "original grant policy changed; refresh and confirm access again"
        );
        validate_identifier(&self.id)?;
        validate_revision(self.grant_revision)?;
        ensure!(
            chrono::DateTime::parse_from_rfc3339(&self.expires_at)? > Utc::now(),
            "original access preview expired; refresh and confirm its members again"
        );
        let mut accounts = BTreeSet::new();
        for member in &self.members {
            validate_identifier(&member.account_id)?;
            validate_identifier(&member.username)?;
            ensure!(
                accounts.insert(&member.account_id),
                "original access preview repeats a member"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginalReader {
    pub account_id: String,
    pub username: Option<String>,
    pub current_member: bool,
    pub owner_default: bool,
    pub committer_default: bool,
    pub explicit_grant: bool,
    pub session_explicit_grant: bool,
    pub session_revoked: bool,
    pub default_suppressed: bool,
    pub session_membership_invalidated: bool,
    pub revoked: bool,
    pub membership_invalidated: bool,
    pub can_read: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantState {
    pub target: GrantTarget,
    pub policy_revision: i64,
    pub grant_revision: i64,
    pub readers: Vec<OriginalReader>,
}

impl GrantState {
    fn validate(&self, expected: &GrantTarget, policy_revision: i64) -> Result<()> {
        self.target.verify(expected)?;
        validate_revision(self.policy_revision)?;
        ensure!(
            self.policy_revision == policy_revision,
            "original grant policy changed; refresh and confirm access again"
        );
        validate_revision(self.grant_revision)?;
        let mut accounts = BTreeSet::new();
        for reader in &self.readers {
            validate_identifier(&reader.account_id)?;
            if let Some(username) = &reader.username {
                validate_identifier(username)?;
            }
            ensure!(
                accounts.insert(&reader.account_id),
                "original access state repeats a reader"
            );
            let access = reader.access();
            access.verify_scope(expected.scope())?;
            if expected.scope() == GrantScope::Session {
                ensure!(
                    !reader.owner_default
                        && !reader.committer_default
                        && !reader.explicit_grant
                        && !reader.revoked
                        && !reader.membership_invalidated,
                    "session grant inventory cannot imply per-commit default access"
                );
            }
            ensure!(
                reader.can_read == access.can_read(),
                "original access state has inconsistent reader eligibility"
            );
        }
        Ok(())
    }
}

impl OriginalReader {
    pub fn access(&self) -> ReaderAccess {
        ReaderAccess {
            current_member: self.current_member,
            owner_default: self.owner_default,
            committer_default: self.committer_default,
            explicit_grant: self.explicit_grant,
            session_explicit_grant: self.session_explicit_grant,
            revoked: self.revoked,
            session_revoked: self.session_revoked,
            default_suppressed: self.default_suppressed,
            membership_invalidated: self.membership_invalidated,
            session_membership_invalidated: self.session_membership_invalidated,
        }
    }
}

impl Client {
    pub fn original_grants(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        target: &GrantTarget,
        policy_revision: i64,
    ) -> Result<GrantState> {
        validate_revision(policy_revision)?;
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query
            .append_pair("scope", target.scope().as_str())
            .append_pair("expected_policy_revision", &policy_revision.to_string());
        if let GrantTarget::Commit {
            published_commit_id,
            envelope_digest,
            ..
        } = target
        {
            query
                .append_pair("published_commit_id", published_commit_id)
                .append_pair("envelope_digest", envelope_digest);
        }
        let query = query.finish();
        let response: GrantState =
            self.with_repository_route(repository, identity, |repository| {
                let path = self.grant_path(repository, identity, target, "original-grants")?;
                self.get_expected(&format!("{path}?{query}"), Some(&identity.agent_id))
            })?;
        response.validate(target, policy_revision)?;
        Ok(response)
    }

    pub fn preview_original_grants(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        target: &GrantTarget,
        policy_revision: i64,
    ) -> Result<GrantPreview> {
        validate_revision(policy_revision)?;
        let response: GrantPreview =
            self.with_repository_route(repository, identity, |repository| {
                let path =
                    self.grant_path(repository, identity, target, "original-grants/preview")?;
                self.post_expected(&path,
                &serde_json::json!({"target":target,"expected_policy_revision":policy_revision}),
                Some(&identity.agent_id))
            })?;
        response.validate(target, policy_revision)?;
        Ok(response)
    }

    pub fn mutate_original_grants(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        mutation: &GrantMutation,
    ) -> Result<GrantResult> {
        mutation.validate()?;
        let response: GrantResult = self
            .with_repository_route(repository, identity, |repository| {
                let path =
                    self.grant_path(repository, identity, &mutation.target, "original-grants")?;
                self.post_expected(&path, mutation, Some(&identity.agent_id))
            })
            .map_err(access_error)?;
        response.verify(mutation)?;
        Ok(response)
    }

    pub(super) fn grant_path(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        target: &GrantTarget,
        operation: &str,
    ) -> Result<String> {
        target.validate()?;
        ensure!(
            target.agent_id() == identity.agent_id,
            "original access request belongs to another repository"
        );
        self.session_privacy_path(repository, identity, target.session_id(), operation)
    }
}

pub(super) fn access_error(error: anyhow::Error) -> anyhow::Error {
    if let Some(api) = error.downcast_ref::<ApiError>() {
        if api.kind == "unsupported" {
            return error;
        }
        if api.status == 409 {
            return error.context("original access policy or grants changed or the request was superseded; inspect access before confirming a new operation");
        }
    }
    error
}
