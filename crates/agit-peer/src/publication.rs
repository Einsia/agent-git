//! Durable publication identity is independent of a reconnect's delivery authority.

use crate::{access::Principal, cloud::ConnectionGrant};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const CONFIRM_PATH: &str = "/api/peer/publications/confirm";
pub const MAX_NOTIFICATION_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Executor {
    pub owner: Principal,
    pub device_id: String,
    pub credential_epoch: u64,
}

/// Coverage applies only to this captured stream incarnation, never a resumed live stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    pub session_id: String,
    pub native_session_id: String,
    pub runtime: String,
    pub generation: u64,
    #[serde(default)]
    pub incarnation: Option<String>,
    #[serde(default)]
    pub through_seq: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    pub version: u32,
    pub notification_id: String,
    pub executor: Executor,
    pub repository_id: String,
    pub branch: String,
    pub public_commit: String,
    pub projected_session_id: String,
    pub capture: Capture,
}

impl Notification {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported publication notification version"
        );
        for value in [
            &self.notification_id,
            &self.executor.owner.issuer,
            &self.executor.owner.account_id,
            &self.executor.device_id,
            &self.repository_id,
            &self.branch,
            &self.capture.session_id,
            &self.capture.native_session_id,
            &self.capture.runtime,
        ] {
            bounded_text(value)?;
        }
        ensure!(oid(&self.public_commit), "invalid public commit ID");
        ensure!(
            self.projected_session_id
                .strip_prefix("agit-")
                .is_some_and(|id| id.len() == 40 && oid(id)),
            "invalid projected session identity"
        );
        if let Some(incarnation) = &self.capture.incarnation {
            bounded_text(incarnation)?;
        }
        ensure!(
            self.capture.through_seq.is_none()
                || (self.capture.incarnation.is_some() && self.capture.generation > 0),
            "publication sequence coverage needs a captured stream incarnation"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_NOTIFICATION_BYTES,
            "publication notification exceeds its size limit"
        );
        Ok(())
    }

    /// The ordered array is the cross-language canonical binding, regardless of object key order.
    pub fn digest(&self) -> Result<String> {
        self.validate()?;
        let canonical = serde_json::json!([
            "agit-rc-publication-v1",
            self.notification_id,
            [
                self.executor.owner.issuer,
                self.executor.owner.account_id,
                self.executor.device_id,
                self.executor.credential_epoch
            ],
            [
                self.repository_id,
                self.branch,
                self.public_commit,
                self.projected_session_id
            ],
            [
                self.capture.session_id,
                self.capture.native_session_id,
                self.capture.runtime,
                self.capture.generation,
                self.capture.incarnation,
                self.capture.through_seq
            ],
        ]);
        Ok(format!(
            "sha256:{}",
            hex::encode(Sha256::digest(serde_json::to_vec(&canonical)?))
        ))
    }

    /// A received grant still requires the receiver's independent current-authority lookup.
    pub fn authorize(&self, grant: &ConnectionGrant, now_ms: i64) -> Result<()> {
        self.validate()?;
        let scope = grant
            .session_controller
            .as_ref()
            .context("publication delivery requires a session controller")?;
        ensure!(
            grant.project_controller.is_none()
                && grant.expires_at_ms > now_ms
                && scope.generation > 0
                && scope.access.can_control()
                && grant.caller == self.executor.owner
                && grant.source.owner == self.executor.owner
                && grant.target.owner == self.executor.owner
                && grant.target.id == self.executor.device_id
                && grant.target.credential_epoch == self.executor.credential_epoch
                && scope.runtime == self.capture.runtime
                && scope.session_id == self.capture.native_session_id,
            "publication delivery authority does not match the notification"
        );
        bounded_text(&grant.id)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delivery {
    pub grant_id: String,
    pub controller_generation: u64,
    pub notification: Notification,
}

impl Delivery {
    pub fn new(notification: Notification, grant: &ConnectionGrant, now_ms: i64) -> Result<Self> {
        notification.authorize(grant, now_ms)?;
        Ok(Self {
            grant_id: grant.id.clone(),
            controller_generation: grant.session_controller.as_ref().unwrap().generation,
            notification,
        })
    }

    pub fn validate(&self, grant: &ConnectionGrant, now_ms: i64) -> Result<()> {
        self.notification.authorize(grant, now_ms)?;
        ensure!(
            self.grant_id == grant.id
                && Some(self.controller_generation)
                    == grant
                        .session_controller
                        .as_ref()
                        .map(|scope| scope.generation),
            "publication delivery belongs to another connection"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub version: u32,
    pub receipt_id: String,
    pub notification_id: String,
    pub binding_digest: String,
    pub repository_id: String,
    pub public_commit: String,
}

impl Receipt {
    pub fn validate(&self, notification: &Notification) -> Result<()> {
        bounded_text(&self.receipt_id)?;
        ensure!(
            self.version == 1
                && self.notification_id == notification.notification_id
                && self.binding_digest == notification.digest()?
                && self.repository_id == notification.repository_id
                && self.public_commit == notification.public_commit,
            "publication receipt does not match the immutable notification"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Acknowledgement {
    pub grant_id: String,
    pub controller_generation: u64,
    pub receipt: Receipt,
}

impl Acknowledgement {
    pub fn validate(
        &self,
        delivery: &Delivery,
        grant: &ConnectionGrant,
        now_ms: i64,
    ) -> Result<()> {
        delivery.validate(grant, now_ms)?;
        ensure!(
            self.grant_id == delivery.grant_id
                && self.controller_generation == delivery.controller_generation,
            "publication acknowledgement belongs to another connection"
        );
        self.receipt.validate(&delivery.notification)
    }
}

fn bounded_text(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control),
        "invalid publication identity field"
    );
    Ok(())
}

fn oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{
        access::Access,
        cloud::{Device, SessionController},
    };

    pub(crate) fn fixture(issuer: &str) -> (Notification, ConnectionGrant) {
        let owner = Principal {
            issuer: issuer.into(),
            account_id: "owner".into(),
        };
        let device = Device {
            id: "executor".into(),
            owner: owner.clone(),
            machine_id: "machine".into(),
            display_name: "Fixture".into(),
            credential_epoch: 1,
            certificate: crate::Identity::generate().unwrap().certificate().clone(),
        };
        let grant = ConnectionGrant {
            id: "grant".into(),
            caller: owner.clone(),
            source: Device {
                id: "controller".into(),
                ..device.clone()
            },
            target: device,
            expires_at_ms: i64::MAX,
            session_controller: Some(SessionController {
                session_id: "native".into(),
                runtime: "claude-code".into(),
                generation: 7,
                access: Access::Admin,
            }),
            project_controller: None,
        };
        let notification = Notification {
            version: 1,
            notification_id: "notification".into(),
            executor: Executor {
                owner,
                device_id: "executor".into(),
                credential_epoch: 1,
            },
            repository_id: "repository".into(),
            branch: "s/work".into(),
            public_commit: "a".repeat(40),
            projected_session_id: format!("agit-{}", "b".repeat(40)),
            capture: Capture {
                session_id: "logical".into(),
                native_session_id: "native".into(),
                runtime: "claude-code".into(),
                generation: 3,
                incarnation: Some("incarnation".into()),
                through_seq: Some(17),
            },
        };
        (notification, grant)
    }

    pub(crate) fn ack(delivery: &Delivery) -> Acknowledgement {
        Acknowledgement {
            grant_id: delivery.grant_id.clone(),
            controller_generation: delivery.controller_generation,
            receipt: Receipt {
                version: 1,
                receipt_id: "receipt".into(),
                notification_id: delivery.notification.notification_id.clone(),
                binding_digest: delivery.notification.digest().unwrap(),
                repository_id: delivery.notification.repository_id.clone(),
                public_commit: delivery.notification.public_commit.clone(),
            },
        }
    }

    /// Renewal preserves idempotency while old responses and changed authority cannot acknowledge it.
    #[test]
    fn publication_receipt_binds_content_and_current_delivery_authority() {
        let (notification, grant) = fixture("https://hub.example");
        assert_eq!(
            notification.digest().unwrap(),
            "sha256:3217ab34cb9be6e76f79fac0491c92cbef64406406d8c9f84a3748739351f940"
        );
        let delivery = Delivery::new(notification.clone(), &grant, 0).unwrap();
        let acknowledged = ack(&delivery);
        acknowledged.validate(&delivery, &grant, 0).unwrap();
        let mut renewed = grant.clone();
        renewed.id = "renewed".into();
        renewed.session_controller.as_mut().unwrap().generation += 1;
        let retry = Delivery::new(notification.clone(), &renewed, 0).unwrap();
        assert_eq!(ack(&retry).receipt, acknowledged.receipt);
        assert!(acknowledged.validate(&retry, &renewed, 0).is_err());
        ack(&retry).validate(&retry, &renewed, 0).unwrap();
        assert!(ack(&retry).validate(&retry, &renewed, i64::MAX).is_err());
        let mut changed = notification.clone();
        changed.public_commit = "c".repeat(40);
        assert!(acknowledged.receipt.validate(&changed).is_err());
        changed = notification.clone();
        changed.capture.through_seq = Some(18);
        assert!(acknowledged.receipt.validate(&changed).is_err());
        changed.capture.incarnation = None;
        assert!(changed.validate().is_err());
        for mutation in 0..4 {
            let mut wrong = grant.clone();
            match mutation {
                0 => wrong.target.credential_epoch += 1,
                1 => wrong.caller.account_id = "other-account".into(),
                2 => wrong.session_controller.as_mut().unwrap().session_id = "other-session".into(),
                _ => wrong.session_controller.as_mut().unwrap().access = Access::Read,
            }
            assert!(Delivery::new(notification.clone(), &wrong, 0).is_err());
        }
        let mut wire = serde_json::to_value(notification).unwrap();
        wire["source_commit"] = serde_json::json!("private");
        assert!(serde_json::from_value::<Notification>(wire).is_err());
    }
}
