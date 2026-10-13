//! Receive receipts bind confirmed public Git objects to a complete remote ref transition.

use crate::domain::privacy::POLICY_VERSION;
use crate::domain::privacy_envelope::{digest_bytes, valid_token, validate_digest};
use crate::domain::session_privacy::{SESSION_ENVELOPE_VERSION, SessionPublicationManifest};
use crate::hub::Client;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct MandatoryPolicy {
    version: u32,
    source: String,
    require_envelope: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    require_session_config: bool,
    publication_format_version: u32,
    protected_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content_policy_digest: Option<String>,
}

#[derive(Deserialize)]
struct Strategy {
    agent_id: String,
    policy_version: u32,
    policy_digest: String,
    publication_format_version: u32,
    mandatory_policy_digest: String,
    mandatory_policy: MandatoryPolicy,
}

pub(crate) struct PublicationPolicy {
    path: String,
    policy_digest: String,
    mandatory_policy_digest: String,
    receiver_scope: String,
    session_manifest: SessionPublicationManifest,
}

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Binding {
    snapshot_digest: String,
    policy_digest: String,
    receiver_scope: String,
    envelope_format_version: u32,
    expected_refs_digest: String,
    target_refs_digest: String,
}

#[derive(Deserialize)]
struct Confirmation {
    preview_id: String,
    #[serde(flatten)]
    binding: Binding,
    mandatory_policy_digest: String,
}

#[derive(Deserialize)]
struct Preview {
    #[serde(flatten)]
    confirmation: Confirmation,
    expires_at: String,
}

pub(crate) struct ReceiveReceipt {
    id: String,
    scope: String,
}

impl ReceiveReceipt {
    pub(crate) fn headers(&self) -> [(&'static str, &str); 2] {
        [
            ("X-AgentGit-Privacy-Preview-Id", &self.id),
            ("X-AgentGit-Privacy-Receiver-Scope", &self.scope),
        ]
    }
}

impl PublicationPolicy {
    pub(crate) fn register_sessions(
        client: &Client,
        repository: &str,
        identity: &crate::hub::identity::RemoteIdentity,
        policy_digest: &str,
        manifest: &SessionPublicationManifest,
        visibility: &str,
    ) -> Result<Self> {
        manifest.validate()?;
        validate_digest(policy_digest)?;
        ensure!(
            client.has_token()
                && crate::domain::session_privacy::canonical_hub(client.base())? == identity.hub
                && manifest.repository_id == identity.agent_id,
            "session publication requires its authenticated repository authority"
        );
        ensure!(
            matches!(visibility, "public" | "private"),
            "unsupported publication audience"
        );
        let path = super::repository_path(repository)?;
        let strategy: Strategy = client.put(
            &format!("{path}/strategy"),
            &json!({
                "expected_repository_id": identity.agent_id,
                "policy_version": POLICY_VERSION,
                "policy_digest": policy_digest,
                "publication_format_version": SESSION_ENVELOPE_VERSION,
                "summary": {"session_modes": ["plain", "session_owner_v2"]},
            }),
        )?;
        ensure!(
            strategy.agent_id == identity.agent_id
                && strategy.policy_version == POLICY_VERSION
                && strategy.policy_digest == policy_digest
                && strategy.publication_format_version == SESSION_ENVELOPE_VERSION,
            "Hub strategy differs from the confirmed session publication"
        );
        let mandatory = &strategy.mandatory_policy;
        ensure!(
            mandatory.version == POLICY_VERSION
                && !mandatory.require_envelope
                && mandatory.require_session_config
                && mandatory.publication_format_version == SESSION_ENVELOPE_VERSION
                && matches!(mandatory.source.as_str(), "repository" | "organization")
                && strategy.mandatory_policy_digest
                    == digest_bytes(&serde_json::to_vec(mandatory)?),
            "Hub does not enforce registered session publication modes"
        );
        Ok(Self {
            path,
            policy_digest: policy_digest.into(),
            mandatory_policy_digest: strategy.mandatory_policy_digest,
            receiver_scope: format!("sessions:{visibility}:{}", manifest.repository_id),
            session_manifest: manifest.clone(),
        })
    }

    fn binding(
        &self,
        before: &BTreeMap<String, String>,
        after: &BTreeMap<String, String>,
    ) -> Result<Binding> {
        let target_refs_digest = refs_digest(after);
        Ok(Binding {
            snapshot_digest: digest_bytes(&serde_json::to_vec(&self.session_manifest)?),
            policy_digest: self.policy_digest.clone(),
            receiver_scope: self.receiver_scope.clone(),
            envelope_format_version: SESSION_ENVELOPE_VERSION,
            expected_refs_digest: refs_digest(before),
            target_refs_digest,
        })
    }

    pub(crate) fn confirm(
        &self,
        client: &Client,
        before: &BTreeMap<String, String>,
        after: &BTreeMap<String, String>,
    ) -> Result<ReceiveReceipt> {
        let binding = self.binding(before, after)?;
        let mut request = serde_json::to_value(&binding)?;
        request["manifest"] = serde_json::to_value(&self.session_manifest)?;
        let preview: Preview =
            client.post(&format!("{}/publication/preview", self.path), &request)?;
        self.verify(&preview.confirmation, &binding, None)?;
        let expires = chrono::DateTime::parse_from_rfc3339(&preview.expires_at)
            .context("invalid publication receipt expiry")?;
        ensure!(
            expires > chrono::Utc::now(),
            "publication receipt has expired"
        );
        request["preview_id"] = json!(preview.confirmation.preview_id);
        let confirmed: Confirmation =
            client.post(&format!("{}/publication/confirm", self.path), &request)?;
        self.verify(&confirmed, &binding, Some(&preview.confirmation.preview_id))?;
        ensure!(
            expires > chrono::Utc::now(),
            "publication receipt has expired"
        );
        Ok(ReceiveReceipt {
            id: confirmed.preview_id,
            scope: self.receiver_scope.clone(),
        })
    }

    fn verify(&self, response: &Confirmation, binding: &Binding, id: Option<&str>) -> Result<()> {
        ensure!(
            !response.preview_id.is_empty()
                && response.preview_id.len() <= 128
                && valid_token(&response.preview_id)
                && id.is_none_or(|id| id == response.preview_id)
                && response.binding == *binding
                && response.mandatory_policy_digest == self.mandatory_policy_digest,
            "publication receipt differs from the confirmed content, policy or recipient"
        );
        Ok(())
    }
}

pub(crate) fn refs_digest(refs: &BTreeMap<String, String>) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for (name, oid) in refs {
        digest.update(name.as_bytes());
        digest.update(b"\0");
        digest.update(oid.as_bytes());
        digest.update(b"\n");
    }
    format!("sha256:{}", hex::encode(digest.finalize()))
}

pub(crate) fn parse_refs(output: &str) -> Result<BTreeMap<String, String>> {
    ensure!(
        output.len() <= 4 * 1024 * 1024,
        "publication ref advertisement is too large"
    );
    let mut refs = BTreeMap::new();
    for line in output.lines() {
        let (oid, name) = line
            .split_once('\t')
            .context("invalid publication ref advertisement")?;
        ensure!(
            matches!(oid.len(), 40 | 64)
                && oid
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                && name.starts_with("refs/")
                && !name.contains([' ', '\\', '~', '^', ':', '?', '*', '['])
                && !name.chars().any(char::is_control)
                && !name.contains("..")
                && !name.contains("@{")
                && !name.ends_with('.')
                && name.split('/').all(|part| !part.is_empty()
                    && !part.starts_with('.')
                    && !part.ends_with(".lock"))
                && refs.len() < 4096
                && refs.insert(name.into(), oid.into()).is_none(),
            "invalid or duplicate publication ref advertisement"
        );
    }
    Ok(refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The server must confirm the same manifest even when the ref targets are unchanged.
    #[test]
    fn session_receive_receipts_bind_manifest_authority_and_ref_transitions() {
        use crate::domain::session_privacy::{AcceptedSnapshot, PublishedSnapshot};
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/privacy-session-owner-envelope.json"
        ))
        .unwrap();
        let snapshot: AcceptedSnapshot =
            serde_json::from_value(fixture["content_key_response"]["snapshot"].clone()).unwrap();
        let manifest = SessionPublicationManifest {
            version: SESSION_ENVELOPE_VERSION,
            repository_id: snapshot.binding.repository_id.clone(),
            snapshots: vec![
                PublishedSnapshot::SessionOwnerV2 {
                    snapshot: snapshot.clone(),
                    config_revision: 1,
                },
                PublishedSnapshot::Plain {
                    commit: "b".repeat(40),
                    session_id: format!("agit-{}", "b".repeat(40)),
                    config_revision: 1,
                },
            ],
            preserved_commits: vec!["a".repeat(40)],
        };
        let mandatory = MandatoryPolicy {
            version: POLICY_VERSION,
            source: "repository".into(),
            require_envelope: false,
            require_session_config: true,
            publication_format_version: SESSION_ENVELOPE_VERSION,
            protected_paths: vec![],
            content_policy_digest: None,
        };
        let mut expected = PublicationPolicy {
            path: "api/agents/team/notes/privacy".into(),
            policy_digest: digest_bytes(b"policy"),
            mandatory_policy_digest: digest_bytes(&serde_json::to_vec(&mandatory).unwrap()),
            receiver_scope: format!("sessions:private:{}", manifest.repository_id),
            session_manifest: manifest.clone(),
        };
        let before = BTreeMap::from([("refs/heads/legacy".into(), "a".repeat(40))]);
        let mut after = before.clone();
        after.insert("refs/heads/work".into(), snapshot.published_commit.clone());
        let binding = expected.binding(&before, &after).unwrap();
        let mut confirmed = serde_json::to_value(&binding).unwrap();
        confirmed["preview_id"] = json!("session-preview");
        confirmed["mandatory_policy_digest"] = json!(expected.mandatory_policy_digest);
        let mut preview = confirmed.clone();
        preview["expires_at"] =
            json!((chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339());
        let strategy = json!({
            "agent_id":manifest.repository_id,
            "policy_version":POLICY_VERSION,
            "policy_digest":expected.policy_digest,
            "publication_format_version":SESSION_ENVELOPE_VERSION,
            "mandatory_policy_digest":expected.mandatory_policy_digest,
            "mandatory_policy":mandatory,
        });
        let (anonymous, identity, worker) = super::super::test_server::server(vec![
            (200, strategy),
            (200, preview),
            (200, confirmed.clone()),
        ]);
        let client = Client::for_hub_with_token(anonymous.base(), "synthetic-session-token");
        let policy = PublicationPolicy::register_sessions(
            &client,
            "team/notes",
            &identity,
            &expected.policy_digest,
            &manifest,
            "private",
        )
        .unwrap();
        let receipt = policy.confirm(&client, &before, &after).unwrap();
        assert_eq!(receipt.headers()[0].1, "session-preview");
        assert_eq!(receipt.headers()[1].1, expected.receiver_scope);
        let requests = worker.join().unwrap();
        assert!(requests[0].starts_with("PUT /api/agents/team/notes/privacy/strategy "));
        for (request, operation) in requests[1..].iter().zip(["preview", "confirm"]) {
            assert!(request.starts_with(&format!(
                "POST /api/agents/team/notes/privacy/publication/{operation} "
            )));
            assert!(request.contains("Bearer synthetic-session-token"));
            let body: serde_json::Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(body["manifest"], serde_json::to_value(&manifest).unwrap());
            assert_eq!(body["snapshot_digest"], binding.snapshot_digest);
            assert_eq!(body["expected_refs_digest"], refs_digest(&before));
            assert_eq!(body["target_refs_digest"], refs_digest(&after));
            assert_eq!(body["envelope_format_version"], SESSION_ENVELOPE_VERSION);
        }
        let response = serde_json::from_value(confirmed).unwrap();
        if let PublishedSnapshot::SessionOwnerV2 { snapshot, .. } =
            &mut expected.session_manifest.snapshots[0]
        {
            snapshot.binding.committer =
                crate::domain::session_privacy::CommitterAttribution::Account {
                    account_id: "another-writer".into(),
                };
        }
        let changed = expected.binding(&before, &after).unwrap();
        assert_ne!(binding.snapshot_digest, changed.snapshot_digest);
        assert!(
            expected
                .verify(&response, &changed, Some("session-preview"))
                .is_err()
        );
        after.insert("refs/heads/other".into(), "c".repeat(40));
        assert!(
            policy
                .verify(
                    &response,
                    &policy.binding(&before, &after).unwrap(),
                    Some("session-preview")
                )
                .is_err()
        );
    }

    #[test]
    fn receipt_requires_every_confirmed_binding_and_a_safe_header() {
        let policy = PublicationPolicy {
            path: "api/agents/alice/app/privacy".into(),
            policy_digest: digest_bytes(b"policy"),
            mandatory_policy_digest: digest_bytes(b"mandatory"),
            receiver_scope: "sessions:private:00000000-0000-0000-0000-000000000001".into(),
            session_manifest: SessionPublicationManifest {
                version: SESSION_ENVELOPE_VERSION,
                repository_id: "00000000-0000-0000-0000-000000000001".into(),
                snapshots: vec![],
                preserved_commits: vec!["a".repeat(40)],
            },
        };
        let binding = Binding {
            snapshot_digest: digest_bytes(b"snapshot"),
            policy_digest: policy.policy_digest.clone(),
            receiver_scope: policy.receiver_scope.clone(),
            envelope_format_version: SESSION_ENVELOPE_VERSION,
            expected_refs_digest: digest_bytes(b"before"),
            target_refs_digest: digest_bytes(b"after"),
        };
        let mut value = serde_json::to_value(&binding).unwrap();
        value["preview_id"] = json!("preview-1");
        value["mandatory_policy_digest"] = json!(policy.mandatory_policy_digest);
        let response = serde_json::from_value(value.clone()).unwrap();
        policy
            .verify(&response, &binding, Some("preview-1"))
            .unwrap();
        for field in [
            "snapshot_digest",
            "policy_digest",
            "receiver_scope",
            "expected_refs_digest",
            "target_refs_digest",
            "mandatory_policy_digest",
            "preview_id",
            "envelope_format_version",
        ] {
            let mut changed = value.clone();
            changed[field] = if field == "envelope_format_version" {
                json!(1)
            } else {
                json!("different")
            };
            assert!(
                policy
                    .verify(
                        &serde_json::from_value(changed).unwrap(),
                        &binding,
                        Some("preview-1")
                    )
                    .is_err(),
                "accepted changed {field}"
            );
        }
        value["preview_id"] = json!("preview\r\nAuthorization: other");
        assert!(
            policy
                .verify(&serde_json::from_value(value).unwrap(), &binding, None)
                .is_err()
        );
    }

    #[test]
    fn receipt_ref_snapshot_includes_other_namespaces_and_rejects_ambiguous_advertisements() {
        let oid = "a".repeat(40);
        let refs = parse_refs(&format!(
            "{oid}\trefs/tags/v1\n{oid}\trefs/archives/saved\n{oid}\trefs/heads/work\n"
        ))
        .unwrap();
        let reordered = parse_refs(&format!(
            "{oid}\trefs/heads/work\n{oid}\trefs/tags/v1\n{oid}\trefs/archives/saved\n"
        ))
        .unwrap();
        assert_eq!(refs_digest(&refs), refs_digest(&reordered));
        let mut branches_only = refs.clone();
        branches_only.remove("refs/archives/saved");
        assert_ne!(refs_digest(&refs), refs_digest(&branches_only));
        for invalid in [
            format!("{oid}\trefs/heads/work\n{oid}\trefs/heads/work\n"),
            format!("{oid}\trefs/tags/v1^{{}}\n"),
            format!("{oid}\tHEAD\n"),
            "bad\trefs/heads/work\n".into(),
        ] {
            assert!(parse_refs(&invalid).is_err());
        }
        assert_eq!(
            refs_digest(&BTreeMap::new()),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
