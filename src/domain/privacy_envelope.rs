//! Authenticated public/private publication objects shared with browser readers.
//!
//! Projection and policy checks precede sealing. Successful encryption authenticates bytes;
//! it does not establish that those bytes are appropriate for public disclosure.

use super::privacy_layer::PrivateLayer;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use crypto_box::{PublicKey, SecretKey};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use zeroize::Zeroizing;

pub const ENVELOPE_FORMAT_VERSION: u32 = 1;
pub const MAX_ENVELOPE_BYTES: usize = 8 * 1024 * 1024;
const MAX_PUBLIC_BYTES: usize = 4 * 1024 * 1024;
const MAX_PRIVATE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ATTACHMENT_BYTES: u64 = 64 * 1024 * 1024;
const AES_ALGORITHM: &str = "aes-256-gcm";
const WRAP_ALGORITHM: &str = "x25519-sealed-box";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WrappedObjectKey {
    pub recipient: String,
    pub algorithm: String,
    pub ciphertext: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncryptedPrivatePayload {
    pub algorithm: String,
    pub nonce: String,
    pub ciphertext: String,
    pub wrapped_keys: Vec<WrappedObjectKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub id: String,
    pub digest: String,
    pub size: u64,
    pub kind: AttachmentKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Public,
    PrivateEncrypted,
}

/// A recipient identifier selects the viewing public key; it never contains a secret.
pub struct ViewingRecipient {
    id: String,
    public_key: PublicKey,
}

impl ViewingRecipient {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Bind cached ciphertext to both the recipient identity and the exact public key.
    pub fn fingerprint(&self) -> Result<String> {
        digest_json(
            &serde_json::json!({"id": self.id, "key": STANDARD.encode(self.public_key.as_bytes())}),
        )
    }

    pub fn from_base64(id: String, value: &str) -> Result<Self> {
        ensure!(valid_token(&id), "invalid viewing recipient identifier");
        let bytes: [u8; 32] = decode_bounded(value, 32, "viewing public key")?
            .try_into()
            .map_err(|_| anyhow::anyhow!("viewing public key has an invalid length"))?;
        // Low-order inputs cannot provide a secret shared with the intended recipient.
        ensure!(
            x25519_dalek::x25519([0x42; 32], bytes) != [0; 32],
            "viewing public key is non-contributory"
        );
        Ok(Self {
            id,
            public_key: PublicKey::from(bytes),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivacyEnvelope {
    pub format_version: u32,
    pub policy_digest: String,
    pub snapshot_digest: String,
    pub public_projection: Value,
    pub private_payload: EncryptedPrivatePayload,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

impl PrivacyEnvelope {
    /// Generate a fresh object key and wrap it for the selected viewing key.
    pub fn seal(
        policy_digest: String,
        snapshot_digest: String,
        public_projection: Value,
        private_bytes: &[u8],
        recipient: &ViewingRecipient,
        attachments: Vec<Attachment>,
    ) -> Result<Self> {
        validate_digest(&policy_digest)?;
        validate_digest(&snapshot_digest)?;
        ensure!(
            valid_token(&recipient.id),
            "invalid viewing recipient identifier"
        );
        ensure!(
            private_bytes.len() <= MAX_PRIVATE_BYTES - 16,
            "privacy private payload is too large"
        );
        let mut object_key = Zeroizing::new([0u8; 32]);
        OsRng.fill_bytes(object_key.as_mut());
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let wrapped = recipient
            .public_key
            .seal(&mut OsRng, object_key.as_ref())
            .map_err(|_| anyhow::anyhow!("privacy object key wrapping failed"))?;
        let mut envelope = Self {
            format_version: ENVELOPE_FORMAT_VERSION,
            policy_digest,
            snapshot_digest,
            public_projection,
            private_payload: EncryptedPrivatePayload {
                algorithm: AES_ALGORITHM.into(),
                nonce: STANDARD.encode(nonce),
                ciphertext: String::new(),
                wrapped_keys: vec![WrappedObjectKey {
                    recipient: recipient.id.clone(),
                    algorithm: WRAP_ALGORITHM.into(),
                    ciphertext: STANDARD.encode(wrapped),
                }],
            },
            attachments,
        };
        let aad = envelope.associated_data()?;
        let ciphertext = Aes256Gcm::new_from_slice(object_key.as_ref())?
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: private_bytes,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("privacy private payload encryption failed"))?;
        envelope.private_payload.ciphertext = STANDARD.encode(ciphertext);
        envelope.validate()?;
        Ok(envelope)
    }

    /// Authenticate the complete publication before returning any private bytes.
    pub fn open(&self, viewing_key: &SecretKey) -> Result<Zeroizing<Vec<u8>>> {
        self.validate()?;
        let object_key = self
            .private_payload
            .wrapped_keys
            .iter()
            .find_map(|wrapped| {
                let ciphertext = STANDARD.decode(&wrapped.ciphertext).ok()?;
                viewing_key
                    .unseal(&ciphertext)
                    .ok()
                    .map(Zeroizing::new)
                    .filter(|key| key.len() == 32)
            })
            .context("no privacy object key can be opened with this viewing key")?;
        let nonce = STANDARD.decode(&self.private_payload.nonce)?;
        let ciphertext = STANDARD.decode(&self.private_payload.ciphertext)?;
        let aad = self.associated_data()?;
        Aes256Gcm::new_from_slice(&object_key)?
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &aad,
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| anyhow::anyhow!("privacy private payload authentication failed"))
    }

    pub fn seal_layer(
        policy_digest: String,
        snapshot_digest: String,
        public_projection: Value,
        layer: &PrivateLayer,
        recipient: &ViewingRecipient,
        attachments: Vec<Attachment>,
    ) -> Result<Self> {
        layer.validate()?;
        let bytes = Zeroizing::new(serde_json::to_vec(layer)?);
        Self::seal(
            policy_digest,
            snapshot_digest,
            public_projection,
            &bytes,
            recipient,
            attachments,
        )
    }

    pub fn open_layer(&self, viewing_key: &SecretKey) -> Result<PrivateLayer> {
        ensure!(
            self.attachments.is_empty(),
            "session recovery does not support standalone attachments"
        );
        let bytes = self.open(viewing_key)?;
        let layer: PrivateLayer = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("privacy private payload is not a private layer"))?;
        layer.validate()?;
        Ok(layer)
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_ENVELOPE_BYTES,
            "privacy envelope is too large"
        );
        let envelope: Self = serde_json::from_slice(bytes)
            .context("privacy envelope has invalid or unknown fields")?;
        envelope.validate()?;
        Ok(envelope)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == ENVELOPE_FORMAT_VERSION,
            "unsupported privacy envelope format"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_ENVELOPE_BYTES,
            "privacy envelope is too large"
        );
        validate_digest(&self.policy_digest)?;
        validate_digest(&self.snapshot_digest)?;
        ensure!(
            canonical_json(&self.public_projection)?.len() <= MAX_PUBLIC_BYTES,
            "privacy public projection is too large"
        );
        ensure!(
            self.private_payload.algorithm == AES_ALGORITHM,
            "unsupported privacy payload algorithm"
        );
        ensure!(
            decode_bounded(&self.private_payload.nonce, 12, "privacy nonce")?.len() == 12,
            "privacy nonce has an invalid length"
        );
        ensure!(
            decode_bounded(
                &self.private_payload.ciphertext,
                MAX_PRIVATE_BYTES,
                "privacy ciphertext"
            )?
            .len()
                >= 16,
            "privacy ciphertext is truncated"
        );
        ensure!(
            !self.private_payload.wrapped_keys.is_empty()
                && self.private_payload.wrapped_keys.len() <= 32,
            "privacy payload has an invalid recipient count"
        );
        let mut recipients = BTreeSet::new();
        for wrapped in &self.private_payload.wrapped_keys {
            ensure!(
                valid_token(&wrapped.recipient) && recipients.insert(&wrapped.recipient),
                "privacy recipients must be unique safe tokens"
            );
            ensure!(
                wrapped.algorithm == WRAP_ALGORITHM,
                "unsupported privacy wrapping algorithm"
            );
            ensure!(
                decode_bounded(&wrapped.ciphertext, 80, "wrapped object key")?.len() == 80,
                "wrapped object key has an invalid length"
            );
        }
        ensure!(
            self.attachments.len() <= 4096,
            "privacy envelope has too many attachments"
        );
        let mut ids = BTreeSet::new();
        for attachment in &self.attachments {
            ensure!(
                valid_token(&attachment.id) && ids.insert(&attachment.id),
                "privacy attachment identifiers must be unique safe tokens"
            );
            validate_digest(&attachment.digest)?;
            ensure!(
                attachment.size <= MAX_ATTACHMENT_BYTES,
                "privacy attachment exceeds the size limit"
            );
            ensure!(
                attachment.kind != AttachmentKind::PrivateEncrypted || attachment.size >= 16,
                "encrypted attachment is truncated"
            );
        }
        Ok(())
    }

    /// Browser readers compute this encoding before AEAD decryption.
    pub fn associated_data(&self) -> Result<Vec<u8>> {
        let authenticated = serde_json::json!({
            "format_version": self.format_version,
            "policy_digest": self.policy_digest,
            "snapshot_digest": self.snapshot_digest,
            "public_projection": self.public_projection,
            "attachments": self.attachments,
            "wrapped_keys": self.private_payload.wrapped_keys,
            "algorithm": self.private_payload.algorithm,
        });
        let digest = digest_json(&authenticated)?;
        Ok(format!("agit-privacy-envelope-v1\0{digest}").into_bytes())
    }

    pub fn public_digest(&self) -> Result<String> {
        digest_json(&self.public_projection)
    }
}

pub(crate) fn validate_digest(digest: &str) -> Result<()> {
    let hex = digest
        .strip_prefix("sha256:")
        .context("privacy digest must be sha256-prefixed")?;
    ensure!(
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "privacy digest must contain a lowercase SHA-256 value"
    );
    Ok(())
}

pub fn digest_bytes(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

pub fn digest_json(value: &Value) -> Result<String> {
    Ok(digest_bytes(&canonical_json(value)?))
}

/// Canonical JSON uses UTF-8 key ordering and only browser-exact integer numbers.
pub fn canonical_json(value: &Value) -> Result<Vec<u8>> {
    fn validate(value: &Value) -> Result<()> {
        match value {
            Value::Number(number) => ensure!(
                number
                    .as_i64()
                    .is_some_and(|n| n.unsigned_abs() <= 9_007_199_254_740_991),
                "privacy JSON numbers must be safe integers"
            ),
            Value::Object(map) => {
                for value in map.values() {
                    validate(value)?;
                }
            }
            Value::Array(values) => {
                for value in values {
                    validate(value)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    validate(value)?;
    Ok(serde_json::to_vec(value)?)
}

pub(crate) fn decode_bounded(value: &str, limit: usize, label: &str) -> Result<Vec<u8>> {
    ensure!(
        value.len() <= limit.div_ceil(3) * 4,
        "{label} exceeds the size limit"
    );
    let bytes = STANDARD
        .decode(value)
        .with_context(|| format!("{label} is not standard base64"))?;
    ensure!(bytes.len() <= limit, "{label} exceeds the size limit");
    Ok(bytes)
}

pub(crate) fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> (PrivacyEnvelope, SecretKey) {
        let secret = SecretKey::from([7; 32]);
        let recipient = ViewingRecipient::from_base64(
            "viewer".into(),
            &STANDARD.encode(secret.public_key().as_bytes()),
        )
        .unwrap();
        let envelope = PrivacyEnvelope::seal(
            digest_bytes(b"policy"),
            digest_bytes(b"snapshot"),
            serde_json::json!({"session": "safe\ncontent", "path": "<workspace>/src/main.rs"}),
            b"/Users/alice/private transcript",
            &recipient,
            vec![Attachment {
                id: "attachment-1".into(),
                digest: digest_bytes(b"public attachment"),
                size: 17,
                kind: AttachmentKind::Public,
            }],
        )
        .unwrap();
        (envelope, secret)
    }

    #[test]
    fn recipient_wrapping_recovers_private_bytes_and_binds_publication() {
        let (envelope, secret) = sample();
        let parsed = PrivacyEnvelope::parse(&serde_json::to_vec(&envelope).unwrap()).unwrap();
        assert_eq!(
            parsed.open(&secret).unwrap().as_slice(),
            b"/Users/alice/private transcript"
        );
        assert!(parsed.open(&SecretKey::from([8; 32])).is_err());
        let mut tampered = parsed.clone();
        tampered.public_projection["session"] = Value::String("changed".into());
        assert!(tampered.open(&secret).is_err());
        tampered = parsed.clone();
        tampered.snapshot_digest = digest_bytes(b"another snapshot");
        assert!(tampered.open(&secret).is_err());
        tampered = parsed.clone();
        tampered.attachments[0].digest = digest_bytes(b"changed attachment");
        assert!(tampered.open(&secret).is_err());
        tampered = parsed;
        tampered.private_payload.wrapped_keys[0].recipient = "another-viewer".into();
        assert!(tampered.open(&secret).is_err());
    }

    #[test]
    fn envelope_rejects_ambiguous_or_unbounded_protocol_fields() {
        let (envelope, _) = sample();
        let mut json = serde_json::to_value(&envelope).unwrap();
        json["plaintext"] = Value::String("unexpected".into());
        assert!(PrivacyEnvelope::parse(&serde_json::to_vec(&json).unwrap()).is_err());
        assert!(canonical_json(&serde_json::json!({"number": 1.5})).is_err());
        assert!(canonical_json(&serde_json::json!({"number": 9007199254740992u64})).is_err());
        assert!(ViewingRecipient::from_base64("viewer".into(), &STANDARD.encode([0; 32])).is_err());
        let mut invalid = envelope;
        invalid.private_payload.wrapped_keys.clear();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn opens_browser_sealed_box_and_webcrypto_fixture() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/privacy-browser-envelope.json"
        ))
        .unwrap();
        let private = STANDARD
            .decode(fixture["test_only_viewing_private_key"].as_str().unwrap())
            .unwrap();
        let key = SecretKey::from_slice(&private).unwrap();
        let envelope =
            PrivacyEnvelope::parse(&serde_json::to_vec(&fixture["envelope"]).unwrap()).unwrap();
        assert_eq!(
            STANDARD.encode(envelope.associated_data().unwrap()),
            fixture["aad_base64"]
        );
        assert_eq!(
            STANDARD.encode(envelope.open(&key).unwrap()),
            fixture["private_payload_base64"]
        );
    }
}
