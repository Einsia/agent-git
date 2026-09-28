//! Authorized repository policy uses semantic values independently of local scan waivers.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub const VALUE_IDENTITY_SCHEME: &str = "sha256-v1";

pub fn value_identity(value: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"agentgit:repository-non-secret:v1\0");
    hash.update(value.as_bytes());
    format!("{VALUE_IDENTITY_SCHEME}:{:x}", hash.finalize())
}

pub fn allows(identities: &HashSet<String>, value: &str) -> bool {
    !identities.is_empty() && identities.contains(&value_identity(value))
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionState {
    Active,
    Revoked,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub id: String,
    pub value_identity: String,
    pub state: DecisionState,
    pub revision: u64,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PolicySnapshot {
    pub version: u32,
    pub agent_id: String,
    pub revision: u64,
    pub value_identity_scheme: String,
    pub decisions: Vec<PolicyDecision>,
}

impl PolicySnapshot {
    pub fn validate(&self, agent_id: &str) -> crate::Result<()> {
        anyhow::ensure!(
            self.version == 1 && self.value_identity_scheme == VALUE_IDENTITY_SCHEME,
            "unsupported repository declaration protocol or value identity scheme"
        );
        anyhow::ensure!(
            self.agent_id == agent_id,
            "repository declaration response changed identity"
        );
        anyhow::ensure!(
            self.decisions.len() <= 1024,
            "repository declaration response exceeds its entry limit"
        );
        let mut ids = HashSet::new();
        let mut values = HashSet::new();
        for decision in &self.decisions {
            decision.validate(self.revision)?;
            anyhow::ensure!(
                ids.insert(&decision.id) && values.insert(&decision.value_identity),
                "duplicate repository declaration response"
            );
        }
        Ok(())
    }
}

impl PolicyDecision {
    pub fn validate(&self, revision: u64) -> crate::Result<()> {
        let valid_identity = self
            .value_identity
            .strip_prefix("sha256-v1:")
            .is_some_and(|hash| {
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            });
        anyhow::ensure!(
            !self.id.is_empty()
                && self.id.len() <= 128
                && valid_identity
                && self.revision > 0
                && self.revision <= revision
                && self.reason.as_ref().is_none_or(|r| r.len() <= 1024),
            "invalid repository declaration response"
        );
        Ok(())
    }
}

#[derive(Serialize)]
pub struct PolicyChange {
    pub version: u32,
    pub expected_agent_id: String,
    pub expected_revision: u64,
    #[serde(flatten)]
    pub action: PolicyAction,
}

#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PolicyAction {
    Allow {
        value_identity: String,
        reason: Option<String>,
    },
    Revoke {
        policy_id: String,
    },
}

#[derive(Deserialize)]
pub struct PolicyChangeResponse {
    pub version: u32,
    pub agent_id: String,
    pub revision: u64,
    pub decision: PolicyDecision,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_versioned_and_byte_exact() {
        assert_eq!(
            value_identity("repository-policy-test-value"),
            "sha256-v1:77a1f88fd6c1d6c021d1ad6651fc5cd89974811e57afa9fbcdcc15620f6d2e9b"
        );
        for other in [
            " repository-policy-test-value",
            "repository-policy-test-value\n",
            "Repository-policy-test-value",
        ] {
            assert_ne!(
                value_identity(other),
                value_identity("repository-policy-test-value")
            );
        }
    }

    #[test]
    fn exact_policy_precedes_hit_caps_and_preserves_distinct_overlaps() {
        use super::super::{Policy, scan_text_capped_with_repository_policy};
        let a = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
        let b = "ghp_T8nR4pYw6MkZ2sDc9XjH5bFu7vQe1aGt3iLs";
        let allowed = HashSet::from([value_identity(a)]);
        let text = format!("{}\n{b}", format!("{a}\n").repeat(100));
        let report = scan_text_capped_with_repository_policy(
            &text,
            &HashSet::new(),
            Policy::STRICT,
            1,
            &allowed,
        );
        assert!(!report.hits.is_empty());
        assert!(
            report
                .hits
                .iter()
                .all(|h| h.fingerprint != super::super::fingerprint(a))
        );
        let json = serde_json::to_string(&vec![a, a])
            .unwrap()
            .replace('R', "\\u0052");
        assert!(
            scan_text_capped_with_repository_policy(
                &json,
                &HashSet::new(),
                Policy::STRICT,
                8,
                &allowed
            )
            .hits
            .is_empty()
        );
        let containing = format!("X3Zp9q{a}Y5Lr7w");
        assert!(
            !scan_text_capped_with_repository_policy(
                &containing,
                &HashSet::new(),
                Policy::STRICT,
                8,
                &allowed
            )
            .hits
            .is_empty()
        );
        let outer = format!("token = \"{a}\"");
        let outer_allowed = HashSet::from([value_identity(&outer)]);
        assert!(
            !scan_text_capped_with_repository_policy(
                &outer,
                &HashSet::new(),
                Policy::STRICT,
                8,
                &outer_allowed
            )
            .hits
            .is_empty()
        );
        assert!(
            !scan_text_capped_with_repository_policy(
                &format!("{b} agit:allow-secret"),
                &HashSet::from([b.to_owned()]),
                Policy::STRICT,
                8,
                &allowed
            )
            .hits
            .is_empty()
        );
    }

    #[test]
    fn multiline_declarations_match_semantic_json_values_before_the_hit_cap() {
        use super::super::{Policy, scan_text_capped_with_repository_policy};
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\nAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n-----END OPENSSH PRIVATE KEY-----";
        let b = "ghp_T8nR4pYw6MkZ2sDc9XjH5bFu7vQe1aGt3iLs";
        let allowed = HashSet::from([value_identity(pem)]);
        let scan = |text: &str, cap| {
            scan_text_capped_with_repository_policy(
                text,
                &HashSet::new(),
                Policy::STRICT,
                cap,
                &allowed,
            )
        };
        assert!(
            !scan_text_capped_with_repository_policy(
                pem,
                &HashSet::new(),
                Policy::STRICT,
                8,
                &HashSet::new()
            )
            .hits
            .is_empty()
        );
        assert!(scan(pem, 1).hits.is_empty());
        let json = serde_json::to_string(&serde_json::json!({"private_key": pem})).unwrap();
        assert!(scan(&json, 1).hits.is_empty());
        assert!(scan(&json.replace("\\n", "\\u000a"), 1).hits.is_empty());
        let literal_escapes = serde_json::to_string(&pem.replace('\n', "\\n")).unwrap();
        assert!(!scan(&literal_escapes, 8).hits.is_empty());
        let padded = HashSet::from([value_identity(&pem.replace('\n', "  "))]);
        assert!(
            !scan_text_capped_with_repository_policy(
                &json,
                &HashSet::new(),
                Policy::STRICT,
                8,
                &padded
            )
            .hits
            .is_empty()
        );
        let dense = format!(
            "{}\n{}",
            format!("{json}\n").repeat(20),
            serde_json::to_string(b).unwrap()
        );
        let report = scan(&dense, 1);
        assert_eq!(report.hits.len(), 1);
        assert_eq!(report.hits[0].fingerprint, super::super::fingerprint(b));
        let outer = format!("prefix {pem} suffix");
        let outer_allowed = HashSet::from([value_identity(&outer)]);
        assert!(
            !scan_text_capped_with_repository_policy(
                &serde_json::to_string(&outer).unwrap(),
                &HashSet::new(),
                Policy::STRICT,
                8,
                &outer_allowed
            )
            .hits
            .is_empty()
        );
    }
}
