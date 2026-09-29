//! Remembered viewing keys require explicit opt-in and remain bound to one authorization scope.

use super::{privacy_envelope::decode_bounded, secret_filter::OsKeyStore};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

const SERVICE: &str = "AgentGit privacy unlock";
const MAX_ENTRY_BYTES: usize = 16 * 1024;
pub const MAX_HOURS: u16 = 720;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub hub: String,
    pub recipient: String,
    pub agent_id: String,
    pub viewing_public_key: String,
}

impl Scope {
    pub fn validate(&self) -> Result<()> {
        entry_name(&self.hub, &self.agent_id)?;
        ensure!(
            uuid::Uuid::parse_str(&self.agent_id)?.to_string() == self.agent_id,
            "invalid remembered unlock repository identity"
        );
        super::privacy_key::validate_public_key(
            "x25519",
            &self.viewing_public_key,
            &self.recipient,
        )?;
        Ok(())
    }

    fn entry(&self) -> Result<String> {
        self.validate()?;
        entry_name(&self.hub, &self.agent_id)
    }
}

fn entry_name(hub: &str, agent_id: &str) -> Result<String> {
    ensure!(
        crate::hub::identity::normalize_hub(hub)? == hub,
        "noncanonical remembered unlock Hub"
    );
    ensure!(
        uuid::Uuid::parse_str(agent_id)?.to_string() == agent_id,
        "invalid remembered unlock repository identity"
    );
    // Rotation replaces the repository's slot; the saved recipient still gates reuse.
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
        hub, agent_id,
    ))?)))
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Remembered {
    version: u32,
    scope: Scope,
    saved_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    private_key: String,
}

impl Drop for Remembered {
    fn drop(&mut self) {
        self.private_key.zeroize();
    }
}

pub trait CredentialStore {
    fn get(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>>;
    fn set(&self, name: &str, bytes: &[u8]) -> Result<()>;
    fn delete(&self, name: &str) -> Result<()>;
}

pub struct OsUnlockStore;

fn with_entry<T>(name: &str, operation: impl FnOnce(keyring::Entry) -> Result<T>) -> Result<T> {
    OsKeyStore::with_access(|| {
        let entry = keyring::Entry::new(SERVICE, name)
            .context("cannot open the OS credential store for privacy unlock")?;
        operation(entry)
    })
}

impl CredentialStore for OsUnlockStore {
    fn get(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
        with_entry(name, |entry| match entry.get_secret() {
            Ok(bytes) => Ok(Some(Zeroizing::new(bytes))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error)
                .context("cannot read the remembered privacy key from the OS credential store"),
        })
    }

    fn set(&self, name: &str, bytes: &[u8]) -> Result<()> {
        with_entry(name, |entry| {
            entry
                .set_secret(bytes)
                .context("cannot remember the privacy key in the OS credential store")
        })
    }

    fn delete(&self, name: &str) -> Result<()> {
        with_entry(name, |entry| match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error)
                .context("cannot remove the remembered privacy key from the OS credential store"),
        })
    }
}

pub fn remember(
    store: &impl CredentialStore,
    scope: &Scope,
    key: &Zeroizing<[u8; 32]>,
    hours: u16,
    now: DateTime<Utc>,
) -> Result<()> {
    scope.validate()?;
    ensure!(
        (1..=MAX_HOURS).contains(&hours),
        "remembered unlock lifetime is out of range"
    );
    ensure!(
        STANDARD.encode(x25519_dalek::x25519(
            **key,
            x25519_dalek::X25519_BASEPOINT_BYTES
        )) == scope.viewing_public_key,
        "cannot remember a different viewing key"
    );
    let saved = Remembered {
        version: 2,
        scope: scope.clone(),
        saved_at: now,
        expires_at: now
            .checked_add_signed(Duration::hours(i64::from(hours)))
            .context("remembered unlock expiry is out of range")?,
        private_key: STANDARD.encode(key.as_ref()),
    };
    let bytes = Zeroizing::new(serde_json::to_vec(&saved)?);
    ensure!(
        bytes.len() <= MAX_ENTRY_BYTES,
        "remembered unlock entry exceeds its byte limit"
    );
    store.set(&scope.entry()?, &bytes)
}

pub fn recall(
    store: &impl CredentialStore,
    scope: &Scope,
    now: DateTime<Utc>,
) -> Result<Zeroizing<[u8; 32]>> {
    let name = scope.entry()?;
    let bytes = store
        .get(&name)?
        .context("no remembered key for this repository; unlock with the repository password")?;
    ensure!(
        bytes.len() <= MAX_ENTRY_BYTES,
        "remembered unlock entry exceeds its byte limit"
    );
    let saved: Remembered = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid remembered unlock entry"))?;
    ensure!(
        saved.version == 2 && saved.scope == *scope,
        "remembered unlock scope changed; unlock with the repository password"
    );
    ensure!(
        saved.saved_at <= now
            && saved.expires_at > saved.saved_at
            && saved.expires_at.signed_duration_since(saved.saved_at)
                <= Duration::hours(i64::from(MAX_HOURS)),
        "invalid remembered unlock lifetime"
    );
    if now >= saved.expires_at {
        store.delete(&name)?;
        anyhow::bail!("remembered unlock expired; unlock with the repository password");
    }
    let bytes = Zeroizing::new(decode_bounded(
        &saved.private_key,
        32,
        "remembered viewing key",
    )?);
    ensure!(bytes.len() == 32, "invalid remembered viewing key");
    let mut key = Zeroizing::new([0; 32]);
    key.copy_from_slice(&bytes);
    ensure!(
        STANDARD.encode(x25519_dalek::x25519(
            *key,
            x25519_dalek::X25519_BASEPOINT_BYTES
        )) == scope.viewing_public_key,
        "remembered viewing key does not match its scope"
    );
    Ok(key)
}

pub fn forget(store: &impl CredentialStore, hub: &str, agent_id: &str) -> Result<()> {
    store.delete(&entry_name(hub, agent_id)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, sync::Mutex};

    #[derive(Default)]
    struct MemoryStore(Mutex<BTreeMap<String, Vec<u8>>>);
    impl CredentialStore for MemoryStore {
        fn get(&self, name: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(name)
                .cloned()
                .map(Zeroizing::new))
        }
        fn set(&self, name: &str, bytes: &[u8]) -> Result<()> {
            self.0.lock().unwrap().insert(name.into(), bytes.to_vec());
            Ok(())
        }
        fn delete(&self, name: &str) -> Result<()> {
            self.0.lock().unwrap().remove(name);
            Ok(())
        }
    }

    #[test]
    fn remembered_keys_require_repository_recipient_and_expiry_and_can_be_forgotten() {
        let store = MemoryStore::default();
        let key = Zeroizing::new([17; 32]);
        let now = Utc::now();
        let public = STANDARD.encode(x25519_dalek::x25519(
            *key,
            x25519_dalek::X25519_BASEPOINT_BYTES,
        ));
        let scope = Scope {
            hub: "https://hub.example".into(),
            recipient: super::super::privacy_key::recipient_id(&public),
            agent_id: uuid::Uuid::from_u128(1).to_string(),
            viewing_public_key: public,
        };
        remember(&store, &scope, &key, 1, now).unwrap();
        assert_eq!(*recall(&store, &scope, now).unwrap(), *key);
        for field in ["hub", "identity", "recipient"] {
            let mut changed = scope.clone();
            match field {
                "hub" => changed.hub = "https://other.example".into(),
                "identity" => changed.agent_id = uuid::Uuid::from_u128(2).to_string(),
                "recipient" => {
                    changed.viewing_public_key = STANDARD.encode(x25519_dalek::x25519(
                        [18; 32],
                        x25519_dalek::X25519_BASEPOINT_BYTES,
                    ));
                    changed.recipient =
                        super::super::privacy_key::recipient_id(&changed.viewing_public_key);
                }
                _ => unreachable!(),
            }
            assert!(
                recall(&store, &changed, now).is_err(),
                "scope changed: {field}"
            );
        }
        let renamed = scope.clone();
        assert!(recall(&store, &renamed, now).is_ok());
        assert!(recall(&store, &scope, now + Duration::hours(1)).is_err());
        assert!(store.0.lock().unwrap().is_empty());
        remember(&store, &scope, &key, 1, now).unwrap();
        forget(&store, &scope.hub, &scope.agent_id).unwrap();
        forget(&store, &scope.hub, &scope.agent_id).unwrap();
        assert!(recall(&store, &scope, now).is_err());
        assert!(remember(&store, &scope, &key, 0, now).is_err());
        assert!(remember(&store, &scope, &key, 1, DateTime::<Utc>::MAX_UTC).is_err());
    }
}
