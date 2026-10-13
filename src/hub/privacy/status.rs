//! Display readiness and original authorization describe one exact accepted publication.

use crate::domain::{
    privacy_envelope::{PrivacyEnvelope, digest_bytes},
    session_grants::SessionGrantPolicy,
    session_privacy::{AcceptedSnapshot, SessionScheme, validate_oid},
};
use crate::hub::{Client, identity::RemoteIdentity};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationStatus {
    pub agent_id: String,
    pub session_id: String,
    pub commit: String,
    pub scheme: SessionScheme,
    pub snapshot: Option<AcceptedSnapshot>,
    #[serde(skip_serializing)]
    envelope_bytes: Option<String>,
    pub capabilities: Option<Capabilities>,
    pub grant_policy: Option<SessionGrantPolicy>,
    pub original_status: Option<OriginalStatus>,
    pub display_status: Option<DisplayStatus>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    pub can_read_original: bool,
    pub can_manage_original_grants: bool,
    pub reader: Option<ReaderAccess>,
}

pub use crate::domain::session_grants::ReaderAccess;

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginalStatus {
    SavedEncrypted,
}

#[derive(Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayStatus {
    Ready,
    Pending,
}

impl PublicationStatus {
    fn verify(
        &self,
        identity: &RemoteIdentity,
        session: &str,
        commit: &str,
        expected: Option<&AcceptedSnapshot>,
    ) -> Result<()> {
        ensure!(
            self.agent_id == identity.agent_id,
            super::location::RepositoryMismatch
        );
        ensure!(
            self.session_id == session && self.commit == commit,
            "privacy status belongs to another publication"
        );
        if let Some(policy) = &self.grant_policy {
            policy.verify(
                &identity.agent_id,
                expected.map_or(session, |snapshot| &snapshot.binding.session_id),
            )?;
        }
        match self.scheme {
            SessionScheme::Plain => ensure!(
                expected.is_none()
                    && self.snapshot.is_none()
                    && self.envelope_bytes.is_none()
                    && self.capabilities.is_none()
                    && self.original_status.is_none()
                    && self.display_status.is_none(),
                "plain publication status carries encrypted snapshot state"
            ),
            SessionScheme::SessionOwnerV2 => {
                let snapshot = self
                    .snapshot
                    .as_ref()
                    .context("missing accepted snapshot")?;
                snapshot.validate()?;
                ensure!(
                    Some(snapshot) == expected
                        && snapshot.binding.repository_id == identity.agent_id
                        && snapshot.published_commit == commit
                        && (snapshot.binding.session_id == session
                            || snapshot.binding.published_session_id == session),
                    "privacy status differs from the selected accepted snapshot"
                );
                let bytes = self
                    .envelope_bytes
                    .as_ref()
                    .context("missing accepted envelope")?;
                ensure!(
                    digest_bytes(bytes.as_bytes()) == snapshot.envelope_digest,
                    "privacy status carries a different envelope"
                );
                let envelope = PrivacyEnvelope::parse(bytes.as_bytes())?;
                ensure!(
                    envelope.session_binding.as_ref() == Some(&snapshot.binding),
                    "privacy status envelope differs from its snapshot authority"
                );
                ensure!(
                    self.original_status == Some(OriginalStatus::SavedEncrypted)
                        && self.display_status.is_some(),
                    "privacy status omits original storage or sanitized display state"
                );
                let access = self
                    .capabilities
                    .as_ref()
                    .context("missing viewer capabilities")?;
                if let Some(reader) = &access.reader {
                    let policy = self
                        .grant_policy
                        .as_ref()
                        .context("missing original grant policy")?;
                    reader.verify_scope(policy.grant_scope)?;
                }
                let eligible = access.reader.as_ref().is_some_and(ReaderAccess::can_read);
                ensure!(
                    (!access.can_read_original || eligible)
                        && (!access.can_manage_original_grants
                            || access
                                .reader
                                .as_ref()
                                .is_some_and(|reader| reader.current_member)),
                    "privacy status capabilities contradict current reader eligibility"
                );
            }
        }
        Ok(())
    }
}

impl Client {
    pub fn session_publication_status(
        &self,
        repository: &str,
        identity: &RemoteIdentity,
        session: &str,
        commit: &str,
        expected: Option<&AcceptedSnapshot>,
    ) -> Result<PublicationStatus> {
        validate_oid(commit)?;
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("ref", commit)
            .finish();
        self.with_repository_route(repository, identity, |repository| {
            let path = self.session_read_path(repository, identity, session, "privacy")?;
            let response: PublicationStatus =
                self.get_expected(&format!("{path}?{query}"), Some(&identity.agent_id))?;
            response.verify(identity, session, commit, expected)?;
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// Indexing readiness cannot imply original access or hide a different accepted snapshot.
    #[test]
    fn status_separates_display_readiness_from_exact_snapshot_access() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/privacy-session-owner-envelope.json"
        ))
        .unwrap();
        let snapshot: AcceptedSnapshot =
            serde_json::from_value(fixture["content_key_response"]["snapshot"].clone()).unwrap();
        let identity =
            RemoteIdentity::new("https://hub.test", &snapshot.binding.repository_id).unwrap();
        let value = json!({
            "agent_id":identity.agent_id, "session_id":snapshot.binding.session_id,
            "commit":snapshot.published_commit, "scheme":"session_owner_v2", "snapshot":snapshot,
            "envelope_bytes":fixture["envelope_bytes"], "original_status":"saved_encrypted",
            "grant_policy":{"agent_id":identity.agent_id,"session_id":snapshot.binding.session_id,"grant_scope":"session","policy_revision":1,"grant_revision":1},
            "display_status":"pending", "capabilities":{"can_read_original":false,
                "can_manage_original_grants":false,"reader":null}
        });
        let verify = |value: Value, expected: Option<&AcceptedSnapshot>| {
            serde_json::from_value::<PublicationStatus>(value)?.verify(
                &identity,
                &snapshot.binding.session_id,
                &snapshot.published_commit,
                expected,
            )
        };
        verify(value.clone(), Some(&snapshot)).unwrap();
        let mut ready = value.clone();
        ready["display_status"] = json!("ready");
        verify(ready, Some(&snapshot)).unwrap();
        for (path, replacement) in [
            ("/agent_id", json!("00000000-0000-0000-0000-000000000002")),
            ("/commit", json!("e".repeat(40))),
            ("/snapshot/published_commit", json!("e".repeat(40))),
            ("/envelope_bytes", json!("{}")),
            ("/original_status", Value::Null),
            ("/display_status", Value::Null),
            ("/capabilities/can_read_original", json!(true)),
            ("/capabilities/can_manage_original_grants", json!(true)),
        ] {
            let mut wrong = value.clone();
            *wrong.pointer_mut(path).unwrap() = replacement;
            assert!(
                verify(wrong, Some(&snapshot)).is_err(),
                "accepted changed {path}"
            );
        }
        let mut anonymous = value.clone();
        anonymous["grant_policy"] = Value::Null;
        verify(anonymous, Some(&snapshot)).unwrap();
        let mut session_reader = value.clone();
        session_reader["capabilities"] = json!({"can_read_original":true,"can_manage_original_grants":false,
            "reader":{"current_member":true,"owner_default":false,"committer_default":false,
                "explicit_grant":false,"session_explicit_grant":true,"revoked":false,"session_revoked":false,
                "default_suppressed":true,"membership_invalidated":true,"session_membership_invalidated":false}});
        verify(session_reader.clone(), Some(&snapshot)).unwrap();
        for path in [
            "/capabilities/reader/session_revoked",
            "/capabilities/reader/session_membership_invalidated",
            "/capabilities/reader/revoked",
        ] {
            let mut denied = session_reader.clone();
            *denied.pointer_mut(path).unwrap() = json!(true);
            assert!(verify(denied, Some(&snapshot)).is_err());
        }
        session_reader["grant_policy"]["grant_scope"] = json!("commit");
        assert!(verify(session_reader, Some(&snapshot)).is_err());
        let plain = json!({"agent_id":identity.agent_id,"session_id":snapshot.binding.session_id,
            "commit":snapshot.published_commit,"scheme":"plain"});
        verify(plain.clone(), None).unwrap();
        assert!(verify(plain, Some(&snapshot)).is_err());
        let parsed: PublicationStatus = serde_json::from_value(value).unwrap();
        assert!(
            serde_json::to_value(parsed)
                .unwrap()
                .get("envelope_bytes")
                .is_none()
        );
    }
}
