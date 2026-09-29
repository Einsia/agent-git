//! Authenticated Hub exclusions are scoped, short-lived additions to device policy.

use crate::domain::privacy::{PrivacyPolicy, mandatory::MandatoryPolicy};
use crate::domain::privacy_envelope::{digest_json, valid_token};
use crate::hub::Client;
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Serialize)]
struct Request<'a> {
    version: u32,
    repository: Option<&'a str>,
    agent_id: Option<&'a str>,
    request_id: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Response {
    version: u32,
    hub: String,
    repository: Option<String>,
    agent_id: Option<String>,
    account_id: String,
    request_id: String,
    owner_id: String,
    revision: String,
    issued_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    sources: Vec<MandatoryPolicy>,
}

/// Only a checked authenticated response can supply effective Hub restrictions.
/// Repository JSON and imported sessions cannot construct this authority.
pub struct PolicySources {
    response: Response,
    digest: String,
}

impl Client {
    /// Resolve existing repositories by immutable ID. `None` requires an absent destination,
    /// allowing namespace rules to constrain preparation before repository creation.
    pub fn privacy_policy_sources(
        &self,
        repository: &str,
        agent_id: Option<&str>,
    ) -> Result<PolicySources> {
        super::repository_path(repository)?;
        ensure!(
            agent_id.is_none_or(valid_token),
            "invalid privacy repository identity"
        );
        self.resolve_privacy_sources(Some(repository), agent_id)
    }

    /// Account-scoped publication also covers native sessions without a repository.
    pub fn privacy_account_policy_sources(&self) -> Result<PolicySources> {
        self.resolve_privacy_sources(None, None)
    }

    fn resolve_privacy_sources(
        &self,
        repository: Option<&str>,
        agent_id: Option<&str>,
    ) -> Result<PolicySources> {
        let expected_account = self.credential_account_id();
        // Policy verification must not rewrite the saved identity while reviewing or refusing content.
        let account: crate::hub::Me = self.get("api/auth/me")?;
        ensure!(
            self.credential_username()
                .is_none_or(|name| name == account.username)
                && expected_account
                    .as_ref()
                    .is_none_or(|id| account.account_id.as_ref() == Some(id)),
            "privacy policy account differs from the selected account"
        );
        let account_id = account
            .account_id
            .context("Hub omitted the privacy policy account ID")?;
        let request = Request {
            version: 1,
            repository,
            agent_id,
            request_id: uuid::Uuid::new_v4().to_string(),
        };
        let response = self
            .post("api/privacy/policy-sources/resolve", &request)
            .context("cannot resolve mandatory Hub privacy rules")?;
        PolicySources::checked(response, &request, self.base(), &account_id, Utc::now())
    }
}

impl PolicySources {
    fn checked(
        response: Response,
        request: &Request<'_>,
        hub: &str,
        account_id: &str,
        now: DateTime<Utc>,
    ) -> Result<Self> {
        ensure!(
            serde_json::to_vec(&response)?.len() <= 256 * 1024,
            "Hub privacy sources exceed their byte limit"
        );
        ensure!(
            response.version == 1
                && response.hub == crate::hub::identity::normalize_hub(hub)?
                && response.account_id == account_id
                && valid_token(account_id)
                && response.repository.as_deref() == request.repository
                && response.agent_id.as_deref() == request.agent_id
                && response.request_id == request.request_id
                && valid_token(&response.owner_id),
            "Hub privacy sources differ from the requested account or repository"
        );
        ensure!(
            response.issued_at <= now + Duration::seconds(30)
                && response.expires_at > now
                && response.expires_at > response.issued_at
                && response.expires_at - response.issued_at <= Duration::minutes(5),
            "Hub privacy sources are expired or have an invalid lifetime"
        );
        ensure!(
            !response.revision.is_empty()
                && response.revision.len() <= 256
                && !response.revision.chars().any(char::is_control)
                && response.sources.len() <= 32,
            "invalid Hub privacy source revision or count"
        );
        let mut ids = BTreeSet::new();
        ensure!(
            response.sources.iter().all(|source| ids.insert(&source.id)),
            "duplicate Hub privacy source ID"
        );
        PrivacyPolicy {
            mandatory: response.sources.clone(),
            ..Default::default()
        }
        .validate()?;
        let digest = digest_json(&serde_json::json!({
            "version": response.version,
            "hub": response.hub,
            "account_id": response.account_id,
            "repository": response.repository,
            "owner_id": response.owner_id,
            "revision": response.revision,
            "sources": response.sources,
        }))?;
        Ok(Self { response, digest })
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// Recheck the same authenticated scope before releasing prepared content.
    pub fn refresh(&self, client: &Client) -> Result<()> {
        self.verify_refresh(&client.resolve_privacy_sources(
            self.response.repository.as_deref(),
            self.response.agent_id.as_deref(),
        )?)
    }

    /// Server receipt policy binds organization rule content independently of the current actor.
    pub fn content_digest(&self) -> Result<String> {
        digest_json(&serde_json::json!({
            "version": self.response.version,
            "owner_id": self.response.owner_id,
            "revision": self.response.revision,
            "sources": self.response.sources,
        }))
    }

    pub fn expires_at(&self) -> DateTime<Utc> {
        self.response.expires_at
    }

    /// Add exclusions while retaining device roots, allowlists and higher-priority restrictions.
    /// The marker binds consent even when the Hub authoritatively returns no exclusions.
    pub fn apply(&self, policy: &PrivacyPolicy) -> Result<PrivacyPolicy> {
        let mut policy = policy.clone();
        policy.mandatory.extend(self.additional_rules()?);
        policy.validate()?;
        Ok(policy)
    }

    pub(crate) fn additional_rules(&self) -> Result<Vec<MandatoryPolicy>> {
        ensure!(
            self.response.expires_at > Utc::now(),
            "Hub privacy sources have expired"
        );
        let mut rules = vec![MandatoryPolicy {
            version: 1,
            id: "hub-policy".into(),
            revision: self.digest.clone(),
            exclude: Vec::new(),
            memory_exclude: Vec::new(),
        }];
        rules.extend(self.response.sources.iter().cloned());
        Ok(rules)
    }

    /// A fresh authenticated response must preserve the namespace's identity and restrictions.
    /// Repository creation may replace an absent ID only when the caller verifies the new ID.
    pub fn verify_refresh(&self, refreshed: &Self) -> Result<()> {
        ensure!(
            refreshed.response.expires_at > Utc::now(),
            "Hub privacy sources have expired"
        );
        ensure!(
            self.digest == refreshed.digest
                && self.response.agent_id.as_ref().is_none_or(|id| refreshed
                    .response
                    .agent_id
                    .as_ref()
                    == Some(id)),
            "mandatory Hub privacy rules changed; prepare and confirm a fresh preview"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::privacy::CandidateAction;

    fn fixture() -> (Response, Request<'static>) {
        let now = Utc::now();
        let request = Request {
            version: 1,
            repository: Some("alice/app"),
            agent_id: Some("agent-1"),
            request_id: "request-1".into(),
        };
        let response = Response {
            version: 1,
            hub: "https://hub.example".into(),
            repository: request.repository.map(str::to_owned),
            agent_id: request.agent_id.map(str::to_owned),
            account_id: "account-1".into(),
            request_id: request.request_id.clone(),
            owner_id: "owner-1".into(),
            revision: "revision-1".into(),
            issued_at: now,
            expires_at: now + Duration::minutes(5),
            sources: vec![MandatoryPolicy {
                version: 1,
                id: "organization".into(),
                revision: "rules-1".into(),
                exclude: vec!["src/**".into()],
                memory_exclude: vec!["team.md".into()],
            }],
        };
        (response, request)
    }

    #[test]
    fn authenticated_sources_narrow_local_rules_and_bind_empty_or_changed_policies() {
        let (response, request) = fixture();
        let checked = |response| {
            PolicySources::checked(
                response,
                &request,
                "https://hub.example",
                "account-1",
                Utc::now(),
            )
            .unwrap()
        };
        let sources = checked(response.clone());
        let temp = tempfile::tempdir().unwrap();
        let policy = PrivacyPolicy {
            workspace: Some(temp.path().to_owned()),
            include: vec!["**".into()],
            memory_allow: vec!["**".into()],
            ..Default::default()
        };
        let effective = sources.apply(&policy).unwrap();
        assert_eq!(
            effective
                .evaluate_file(&temp.path().join("src/main.rs"), None)
                .action,
            CandidateAction::Excluded
        );
        assert_eq!(
            effective.evaluate_memory("team.md", None).action,
            CandidateAction::Excluded
        );
        assert_eq!(
            effective
                .evaluate_file(&temp.path().join("README.md"), None)
                .action,
            CandidateAction::Allowed
        );
        assert_eq!(effective.workspace, policy.workspace);
        let mut refreshed = response.clone();
        refreshed.issued_at += Duration::seconds(1);
        refreshed.expires_at += Duration::seconds(1);
        sources.verify_refresh(&checked(refreshed)).unwrap();
        let mut changed = response.clone();
        changed.revision = "revision-2".into();
        assert!(sources.verify_refresh(&checked(changed)).is_err());
        let mut empty = response;
        empty.sources.clear();
        let empty = checked(empty);
        assert_ne!(
            empty.apply(&policy).unwrap().digest().unwrap(),
            policy.digest().unwrap()
        );
        assert!(sources.verify_refresh(&empty).is_err());
    }

    #[test]
    fn source_response_rejects_wrong_identity_replay_expiry_and_authority_expansion() {
        let (response, request) = fixture();
        let value = serde_json::to_value(&response).unwrap();
        for field in ["hub", "repository", "agent_id", "account_id", "request_id"] {
            let mut changed = value.clone();
            changed[field] = serde_json::json!("other");
            assert!(
                PolicySources::checked(
                    serde_json::from_value(changed).unwrap(),
                    &request,
                    "https://hub.example",
                    "account-1",
                    Utc::now()
                )
                .is_err(),
                "accepted changed {field}"
            );
        }
        let mut expired = response.clone();
        expired.expires_at = Utc::now() - Duration::seconds(1);
        assert!(
            PolicySources::checked(
                expired,
                &request,
                "https://hub.example",
                "account-1",
                Utc::now()
            )
            .is_err()
        );
        let mut changed = value;
        changed["sources"][0]["include"] = serde_json::json!(["**"]);
        assert!(serde_json::from_value::<Response>(changed).is_err());
        let mut duplicate = response.clone();
        duplicate.sources.push(response.sources[0].clone());
        assert!(
            PolicySources::checked(
                duplicate,
                &request,
                "https://hub.example",
                "account-1",
                Utc::now()
            )
            .is_err()
        );
    }

    #[test]
    fn account_sources_cannot_be_substituted_for_repository_sources() {
        let (mut response, mut request) = fixture();
        request.repository = None;
        request.agent_id = None;
        let checked = |response| {
            PolicySources::checked(
                response,
                &request,
                "https://hub.example",
                "account-1",
                Utc::now(),
            )
        };
        assert!(checked(response.clone()).is_err());
        response.repository = None;
        assert!(checked(response.clone()).is_err());
        response.agent_id = None;
        let account = checked(response.clone()).unwrap();
        let mut refreshed = response;
        refreshed.revision = "revision-2".into();
        assert!(
            account
                .verify_refresh(&checked(refreshed).unwrap())
                .is_err()
        );
    }

    #[test]
    fn creation_refresh_keeps_namespace_policy_but_cannot_replace_an_existing_repository() {
        let (mut response, mut request) = fixture();
        response.agent_id = None;
        request.agent_id = None;
        let absent = PolicySources::checked(
            response.clone(),
            &request,
            "https://hub.example",
            "account-1",
            Utc::now(),
        )
        .unwrap();
        response.agent_id = Some("created-agent".into());
        assert!(
            PolicySources::checked(
                response.clone(),
                &request,
                "https://hub.example",
                "account-1",
                Utc::now()
            )
            .is_err()
        );
        request.agent_id = Some("created-agent");
        let created = PolicySources::checked(
            response.clone(),
            &request,
            "https://hub.example",
            "account-1",
            Utc::now(),
        )
        .unwrap();
        absent.verify_refresh(&created).unwrap();
        response.agent_id = Some("replacement-agent".into());
        request.agent_id = Some("replacement-agent");
        let replacement = PolicySources::checked(
            response.clone(),
            &request,
            "https://hub.example",
            "account-1",
            Utc::now(),
        )
        .unwrap();
        assert!(created.verify_refresh(&replacement).is_err());
        response.owner_id = "replacement-owner".into();
        let other_owner = PolicySources::checked(
            response,
            &request,
            "https://hub.example",
            "account-1",
            Utc::now(),
        )
        .unwrap();
        assert!(absent.verify_refresh(&other_owner).is_err());
    }
}
