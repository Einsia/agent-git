//! Executor resource policy restricts callers admitted by cloud connection policy.

use anyhow::ensure;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const MAX_RULES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    pub issuer: String,
    pub account_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "id",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Resource {
    Machine,
    Project(String),
    Session(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Deny,
    Read,
    Control,
    Admin,
}

impl Access {
    pub fn can_read(self) -> bool {
        self != Self::Deny
    }

    pub fn can_control(self) -> bool {
        matches!(self, Self::Control | Self::Admin)
    }

    pub fn is_admin(self) -> bool {
        self == Self::Admin
    }

    pub fn role(self) -> &'static str {
        match self {
            Self::Deny => "denied",
            Self::Read => "viewer",
            Self::Control => "operator",
            Self::Admin => "owner",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub principal: Principal,
    pub resource: Resource,
    pub access: Access,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(try_from = "PolicyDocument")]
pub struct Policy {
    version: u32,
    revision: u64,
    rules: Vec<Rule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyDocument {
    version: u32,
    revision: u64,
    rules: Vec<Rule>,
}

impl TryFrom<PolicyDocument> for Policy {
    type Error = anyhow::Error;

    fn try_from(document: PolicyDocument) -> anyhow::Result<Self> {
        let policy = Self {
            version: document.version,
            revision: document.revision,
            rules: document.rules,
        };
        policy.validate()?;
        Ok(policy)
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            version: 1,
            revision: 0,
            rules: Vec::new(),
        }
    }
}

impl Policy {
    pub fn new(revision: u64, rules: Vec<Rule>) -> anyhow::Result<Self> {
        let policy = Self {
            version: 1,
            revision,
            rules,
        };
        policy.validate()?;
        Ok(policy)
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        ensure!(self.version == 1, "unsupported peer access policy version");
        ensure!(
            self.rules.len() <= MAX_RULES,
            "peer access policy exceeds its rule limit"
        );
        let mut keys = HashSet::new();
        for rule in &self.rules {
            for text in [&rule.principal.issuer, &rule.principal.account_id] {
                ensure!(valid_identifier(text), "invalid peer policy principal");
            }
            if let Resource::Project(id) | Resource::Session(id) = &rule.resource {
                ensure!(valid_identifier(id), "invalid peer policy resource");
            }
            ensure!(
                keys.insert((&rule.principal, &rule.resource)),
                "duplicate peer policy rule"
            );
        }
        Ok(())
    }

    /// Resource coordinates come from the executor registry, never caller-supplied lineage.
    pub fn access(
        &self,
        principal: &Principal,
        session: Option<&str>,
        project: Option<&str>,
    ) -> Access {
        let find = |resource: &Resource| {
            self.rules
                .iter()
                .find(|rule| &rule.principal == principal && &rule.resource == resource)
                .map(|rule| rule.access)
        };
        session
            .and_then(|id| find(&Resource::Session(id.into())))
            .or_else(|| project.and_then(|id| find(&Resource::Project(id.into()))))
            .or_else(|| find(&Resource::Machine))
            .unwrap_or(Access::Deny)
    }
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(issuer: &str) -> Principal {
        Principal {
            issuer: issuer.into(),
            account_id: "account-a".into(),
        }
    }

    fn rule(resource: Resource, access: Access) -> Rule {
        Rule {
            principal: principal("https://hub.example"),
            resource,
            access,
        }
    }

    #[test]
    fn connection_identity_does_not_imply_resource_access() {
        let caller = principal("https://hub.example");
        assert_eq!(
            Policy::default().access(&caller, Some("s"), Some("p")),
            Access::Deny
        );
        let policy = Policy::new(
            1,
            vec![rule(Resource::Session("allowed".into()), Access::Read)],
        )
        .unwrap();
        assert_eq!(policy.access(&caller, Some("other"), None), Access::Deny);
        assert_eq!(policy.access(&caller, Some("allowed"), None), Access::Read);
        assert!(!policy.access(&caller, Some("allowed"), None).can_control());
    }

    #[test]
    fn issuer_is_part_of_the_principal_identity() {
        let policy = Policy::new(1, vec![rule(Resource::Machine, Access::Admin)]).unwrap();
        assert_eq!(
            policy.access(&principal("https://other.example"), None, None),
            Access::Deny
        );
    }

    #[test]
    fn explicit_session_rules_override_inherited_project_access() {
        let policy = Policy::new(
            1,
            vec![
                rule(Resource::Machine, Access::Read),
                rule(Resource::Project("p".into()), Access::Control),
                rule(Resource::Session("private".into()), Access::Deny),
            ],
        )
        .unwrap();
        let caller = principal("https://hub.example");
        assert_eq!(
            policy.access(&caller, Some("private"), Some("p")),
            Access::Deny
        );
        assert_eq!(
            policy.access(&caller, Some("other"), Some("p")),
            Access::Control
        );
        assert_eq!(
            policy.access(&caller, Some("other"), Some("elsewhere")),
            Access::Read
        );
    }

    #[test]
    fn ambiguous_or_unknown_policies_fail_closed() {
        let first = rule(Resource::Machine, Access::Read);
        let second = rule(Resource::Machine, Access::Admin);
        assert!(Policy::new(1, vec![first, second]).is_err());
        let policy = Policy::new(1, vec![rule(Resource::Machine, Access::Admin)]).unwrap();
        let mut document = serde_json::to_value(policy).unwrap();
        document["version"] = 2.into();
        assert!(serde_json::from_value::<Policy>(document).is_err());
    }
}
