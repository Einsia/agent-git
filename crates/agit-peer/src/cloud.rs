//! Cloud grants admit authenticated endpoints without assigning session permissions.

use crate::{PeerCertificate, access::Principal};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("[redacted]")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Enrollment {
    pub machine_id: String,
    pub display_name: String,
    pub certificate: PeerCertificate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub id: String,
    pub owner: Principal,
    pub machine_id: String,
    pub display_name: String,
    pub certificate: PeerCertificate,
    pub credential_epoch: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceCredential {
    pub device: Device,
    pub token: Secret,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectionGrant {
    pub id: String,
    pub caller: Principal,
    pub source: Device,
    pub target: Device,
    pub expires_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantedConnection {
    pub grant: ConnectionGrant,
    pub token: Secret,
}

/// Discovery is advisory; a connection grant rechecks authority at admission.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevicePresence {
    pub device: Device,
    pub online: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DevicePage {
    pub devices: Vec<DevicePresence>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialedConnection {
    pub connection: GrantedConnection,
    pub link_id: String,
    pub ticket: Secret,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum PresenceEvent {
    Ready {
        epoch: String,
    },
    Offer {
        link_id: String,
        source_id: String,
        ticket: Secret,
        grant_token: Secret,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataJoin {
    pub ticket: Secret,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataReady {
    pub link_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_do_not_format_secret_payloads() {
        let value = Secret::new("opaque-value".into());
        assert_eq!(format!("{value:?}"), "[redacted]");
        assert_eq!(value.expose(), "opaque-value");
    }
}
