//! Machine identity and the per-hub connection credential.
//!
//! Two files under `~/.agit/rc/`:
//!
//! * `identity.json` — `{ machine_fingerprint, display_name, created_at }`.
//!   Generated once. The fingerprint is what lets the hub upsert the same
//!   `rc_connections` row on every reconnect instead of minting a new one; lose
//!   it (reinstall) and the machine looks new — which is the correct outcome.
//! * `connections/<hub-host-key>.json` — `{ connection_id, token, hub }` (0600).
//!   The token is **not** the user's API token: it is a long-lived, RC-scoped
//!   credential that can be revoked on its own from either side. Prefix
//!   `agit_rc_` so it is recognisable in logs and caught by the secret scanner.

use crate::infra::config;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub machine_fingerprint: String,
    pub display_name: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    pub connection_id: String,
    pub token: String,
    pub hub: String,
    pub created_at: String,
}

fn identity_path() -> crate::Result<PathBuf> {
    Ok(super::rc_dir()?.join("identity.json"))
}

fn connection_path(hub: &str) -> crate::Result<PathBuf> {
    let d = super::rc_dir()?.join("connections");
    std::fs::create_dir_all(&d)?;
    Ok(d.join(format!("{}.json", config::hub_host_key(hub))))
}

/// Load or create the machine identity.
pub fn identity() -> crate::Result<Identity> {
    let p = identity_path()?;
    if let Ok(s) = std::fs::read_to_string(&p)
        && let Ok(id) = serde_json::from_str::<Identity>(&s)
    {
        return Ok(id);
    }
    let id = Identity {
        machine_fingerprint: uuid::Uuid::new_v4().to_string(),
        display_name: super::hostname(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    write_private(&p, &serde_json::to_string_pretty(&id)?)?;
    Ok(id)
}

pub fn set_display_name(name: &str) -> crate::Result<Identity> {
    let mut id = identity()?;
    id.display_name = name.to_string();
    write_private(&identity_path()?, &serde_json::to_string_pretty(&id)?)?;
    Ok(id)
}

pub fn connection(hub: &str) -> Option<Connection> {
    let p = connection_path(hub).ok()?;
    let s = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&s).ok()
}

pub fn save_connection(c: &Connection) -> crate::Result<()> {
    write_private(&connection_path(&c.hub)?, &serde_json::to_string_pretty(c)?)
}

pub fn remove_connection(hub: &str) -> crate::Result<bool> {
    let p = connection_path(hub)?;
    if p.exists() {
        std::fs::remove_file(p)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Write then chmod 0600. Same discipline as `infra::credentials`.
fn write_private(p: &std::path::Path, body: &str) -> crate::Result<()> {
    std::fs::write(p, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_stable_across_reads() {
        let tmp = tempfile::tempdir().unwrap();
        // `AGIT_HOME` is **process-wide** and the tests run multi-threaded: without the lock
        // this test and the ledger probe in `rc::mod` trample each other (one has just pointed
        // it here, the other deletes it), and the symptom is intermittent red that looks
        // unrelated to the change. See `rc::with_agit_home`.
        crate::rc::with_agit_home(tmp.path(), || {
            let a = identity().unwrap();
            let b = identity().unwrap();
            assert_eq!(a.machine_fingerprint, b.machine_fingerprint);
            assert!(!a.display_name.is_empty());
        });
    }
}
