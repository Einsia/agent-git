//! Authenticated public/private publication objects shared with browser readers.
//!
//! Projection and policy checks precede sealing. Successful encryption authenticates bytes;
//! it does not establish that those bytes are appropriate for public disclosure.

use super::privacy_layer::PrivateLayer;
use super::session_privacy::{AcceptedSnapshot, SESSION_ENVELOPE_VERSION, SessionSnapshotBinding};
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use crypto_box::{PublicKey, SecretKey};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

pub const MAX_ENVELOPE_BYTES: usize = 128 * 1024 * 1024;
pub(crate) const MAX_PRIVATE_BYTES: usize = 64 * 1024 * 1024;
const MAX_PUBLIC_BYTES: usize = MAX_PRIVATE_BYTES;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_binding: Option<SessionSnapshotBinding>,
    pub policy_digest: String,
    pub snapshot_digest: String,
    pub public_projection: Value,
    pub private_payload: EncryptedPrivatePayload,
    #[serde(default)]
    pub attachments: Vec<Value>,
}

impl PrivacyEnvelope {
    /// Only the owner key wraps the content key; snapshot readers are server entitlements.
    pub fn seal_session(
        policy_digest: String,
        public_projection: Value,
        private_bytes: &[u8],
        recipient: &ViewingRecipient,
        binding: SessionSnapshotBinding,
    ) -> Result<Self> {
        binding.validate()?;
        ensure!(
            recipient.id == binding.owner_key.recipient_id()?,
            "viewing recipient differs from the session owner key"
        );
        let snapshot_digest = digest_json(&public_projection)?;
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
            format_version: SESSION_ENVELOPE_VERSION,
            session_binding: Some(binding),
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
            attachments: Vec::new(),
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
        self.open_content_key(&object_key)
    }

    /// AEAD authenticates all immutable bindings even when the server unwraps the key.
    pub fn open_content_key(&self, object_key: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        self.validate()?;
        ensure!(
            object_key.len() == 32,
            "privacy content key has an invalid length"
        );
        let nonce = STANDARD.decode(&self.private_payload.nonce)?;
        let ciphertext = STANDARD.decode(&self.private_payload.ciphertext)?;
        let aad = self.associated_data()?;
        Aes256Gcm::new_from_slice(object_key)?
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

    pub fn open_layer(&self, viewing_key: &SecretKey) -> Result<PrivateLayer> {
        self.parse_layer(&self.open(viewing_key)?)
    }

    pub fn open_layer_with_content_key(&self, content_key: &[u8]) -> Result<PrivateLayer> {
        self.parse_layer(&self.open_content_key(content_key)?)
    }

    fn parse_layer(&self, bytes: &[u8]) -> Result<PrivateLayer> {
        ensure!(
            self.attachments.is_empty(),
            "session recovery does not support standalone attachments"
        );
        let layer: PrivateLayer = serde_json::from_slice(bytes)
            .map_err(|_| anyhow::anyhow!("privacy private payload is not a private layer"))?;
        layer.validate()?;
        let binding = self
            .session_binding
            .as_ref()
            .context("session-owner envelope has no snapshot binding")?;
        ensure!(
            layer.metadata.get("session").and_then(Value::as_str) == Some(&binding.session_id),
            "private layer belongs to another logical session"
        );
        use super::session_privacy::{SessionOwner, SessionPrivacy, SessionScheme};
        let privacy: SessionPrivacy = serde_json::from_value(
            layer
                .metadata
                .get("session_privacy")
                .cloned()
                .context("session original has no ownership record")?,
        )?;
        privacy.validate()?;
        ensure!(
            privacy.scheme == SessionScheme::SessionOwnerV2
                && privacy.owner == Some(SessionOwner::User(binding.owner_key.account_id.clone())),
            "session original ownership differs from its publication authority"
        );
        let source = layer
            .source_committer
            .as_ref()
            .context("session original has no committer evidence")?;
        ensure!(
            source.attribution(
                privacy
                    .hub
                    .as_deref()
                    .context("session original has no Hub")?
            )? == binding.committer,
            "session original committer differs from its publication authority"
        );
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
            self.format_version == SESSION_ENVELOPE_VERSION,
            "unsupported privacy envelope format"
        );
        let binding = self
            .session_binding
            .as_ref()
            .context("session-owner envelope has no snapshot binding")?;
        binding.validate()?;
        ensure!(
            self.attachments.is_empty(),
            "session-owner envelopes do not support standalone attachments"
        );
        ensure!(
            self.snapshot_digest == self.public_digest()?,
            "session snapshot digest differs from its public projection"
        );
        ensure!(
            self.private_payload.wrapped_keys.len() == 1
                && self.private_payload.wrapped_keys[0].recipient
                    == binding.owner_key.recipient_id()?,
            "session envelope must wrap only for its owner key"
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
        let wrapped = &self.private_payload.wrapped_keys[0];
        ensure!(
            wrapped.algorithm == WRAP_ALGORITHM,
            "unsupported privacy wrapping algorithm"
        );
        ensure!(
            decode_bounded(&wrapped.ciphertext, 80, "wrapped object key")?.len() == 80,
            "wrapped object key has an invalid length"
        );
        Ok(())
    }

    /// Browser readers compute this encoding before AEAD decryption.
    pub fn associated_data(&self) -> Result<Vec<u8>> {
        let authenticated = serde_json::json!({
            "format_version": self.format_version,
            "session_binding": self.session_binding,
            "policy_digest": self.policy_digest,
            "snapshot_digest": self.snapshot_digest,
            "public_projection": self.public_projection,
            "attachments": self.attachments,
            "wrapped_keys": self.private_payload.wrapped_keys,
            "algorithm": self.private_payload.algorithm,
        });
        let digest = digest_json(&authenticated)?;
        Ok(format!("agit-privacy-envelope-v{}\0{digest}", self.format_version).into_bytes())
    }

    pub fn public_digest(&self) -> Result<String> {
        digest_json(&self.public_projection)
    }
}

/// Request-scoped key delivery. Neither debug output nor serialization exposes the key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentKeyResponse {
    pub snapshot: AcceptedSnapshot,
    content_key: String,
}

impl std::fmt::Debug for ContentKeyResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContentKeyResponse")
            .field("snapshot", &self.snapshot)
            .field("content_key", &"<redacted>")
            .finish()
    }
}

impl Drop for ContentKeyResponse {
    fn drop(&mut self) {
        self.content_key.zeroize();
    }
}

impl ContentKeyResponse {
    /// The caller retains the expected publication across the network request.
    pub fn open(
        &self,
        expected: &AcceptedSnapshot,
        envelope_bytes: &[u8],
    ) -> Result<Zeroizing<Vec<u8>>> {
        let (envelope, key) = self.verify(expected, envelope_bytes)?;
        envelope.open_content_key(&key)
    }

    pub fn open_layer(
        &self,
        expected: &AcceptedSnapshot,
        envelope_bytes: &[u8],
    ) -> Result<PrivateLayer> {
        let (envelope, key) = self.verify(expected, envelope_bytes)?;
        envelope.open_layer_with_content_key(&key)
    }

    fn verify(
        &self,
        expected: &AcceptedSnapshot,
        envelope_bytes: &[u8],
    ) -> Result<(PrivacyEnvelope, Zeroizing<Vec<u8>>)> {
        expected.validate()?;
        self.snapshot.validate()?;
        ensure!(
            self.snapshot == *expected,
            "content-key response differs from the requested snapshot"
        );
        ensure!(
            envelope_bytes.len() <= MAX_ENVELOPE_BYTES
                && digest_bytes(envelope_bytes) == expected.envelope_digest,
            "content-key response does not match the exact envelope bytes"
        );
        let envelope = PrivacyEnvelope::parse(envelope_bytes)?;
        ensure!(
            envelope.format_version == SESSION_ENVELOPE_VERSION
                && envelope.session_binding.as_ref() == Some(&expected.binding),
            "content-key response differs from the authenticated session binding"
        );
        let key = Zeroizing::new(decode_bounded(
            &self.content_key,
            32,
            "privacy content key",
        )?);
        ensure!(key.len() == 32, "privacy content key has an invalid length");
        Ok((envelope, key))
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
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/privacy-session-owner-envelope.json"
        ))
        .unwrap();
        let binding: SessionSnapshotBinding =
            serde_json::from_value(fixture["envelope"]["session_binding"].clone()).unwrap();
        let recipient = ViewingRecipient::from_base64(
            binding.owner_key.recipient_id().unwrap(),
            &STANDARD.encode(secret.public_key().as_bytes()),
        )
        .unwrap();
        let envelope = PrivacyEnvelope::seal_session(
            digest_bytes(b"policy"),
            serde_json::json!({"session": "safe\ncontent", "path": "<workspace>/src/main.rs"}),
            b"/Users/alice/private transcript",
            &recipient,
            binding,
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
        tampered
            .attachments
            .push(serde_json::json!({"id":"attachment"}));
        assert!(tampered.open(&secret).is_err());
        tampered = parsed;
        tampered.private_payload.wrapped_keys[0].recipient = "another-viewer".into();
        assert!(tampered.open(&secret).is_err());
    }

    /// Large display projections and originals retain both sequences through envelope recovery.
    #[test]
    fn large_session_originals_round_trip_with_both_sequences() {
        let (sample, secret) = sample();
        let binding = sample.session_binding.unwrap();
        let metadata = super::super::meta::Meta::new(
            binding.session_id.clone(),
            "codex".into(),
            "/synthetic/workspace".into(),
        );
        let native = serde_json::json!({"type":"message", "content":"x".repeat(4 * 1024 * 1024)});
        let log =
            super::super::transcript::wrap_lines(&native.to_string(), "codex", &binding.session_id);
        let layer = PrivateLayer::new(
            &log,
            &log,
            serde_json::to_value(metadata).unwrap(),
            Default::default(),
        )
        .unwrap();
        let bytes = serde_json::to_vec(&layer).unwrap();
        let recipient = ViewingRecipient::from_base64(
            binding.owner_key.recipient_id().unwrap(),
            &STANDARD.encode(secret.public_key().as_bytes()),
        )
        .unwrap();
        let envelope = PrivacyEnvelope::seal_session(
            sample.policy_digest,
            serde_json::json!({"session": {"log": &log, "view": &log}}),
            &bytes,
            &recipient,
            binding,
        )
        .unwrap();
        let parsed = PrivacyEnvelope::parse(&serde_json::to_vec(&envelope).unwrap()).unwrap();
        assert_eq!(parsed.public_projection["session"]["log"], log);
        assert_eq!(parsed.public_projection["session"]["view"], log);
        let restored: PrivateLayer =
            serde_json::from_slice(&parsed.open(&secret).unwrap()).unwrap();
        restored.validate().unwrap();
        let (restored_log, restored_view) = restored.session_bytes().unwrap();
        assert_eq!(*restored_log, log);
        assert_eq!(*restored_view, log);
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

    /// A delivered key authorizes one immutable snapshot, while AEAD binds its source and owner.
    #[test]
    fn session_owner_delivery_requires_exact_snapshot_and_authenticated_bindings() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/privacy-session-owner-envelope.json"
        ))
        .unwrap();
        let response: ContentKeyResponse =
            serde_json::from_value(fixture["content_key_response"].clone()).unwrap();
        let expected = response.snapshot.clone();
        let bytes = fixture["envelope_bytes"].as_str().unwrap().as_bytes();
        let envelope = PrivacyEnvelope::parse(bytes).unwrap();
        let content_key = Zeroizing::new(
            STANDARD
                .decode(
                    fixture["content_key_response"]["content_key"]
                        .as_str()
                        .unwrap(),
                )
                .unwrap(),
        );
        assert_eq!(
            STANDARD.encode(response.open(&expected, bytes).unwrap()),
            fixture["private_payload_base64"]
        );
        assert_eq!(
            STANDARD.encode(envelope.associated_data().unwrap()),
            fixture["aad_base64"]
        );
        assert!(
            !format!("{response:?}").contains(
                fixture["content_key_response"]["content_key"]
                    .as_str()
                    .unwrap()
            )
        );
        let mut other = expected.clone();
        other.published_commit = "e".repeat(40);
        assert!(response.open(&other, bytes).is_err());
        let mut different_bytes = bytes.to_vec();
        different_bytes.push(b'\n');
        assert!(response.open(&expected, &different_bytes).is_err());

        for (path, value) in [
            (
                "/session_binding/repository_id",
                Value::String("00000000-0000-0000-0000-000000000002".into()),
            ),
            (
                "/session_binding/session_id",
                Value::String(format!("agit-{}", "e".repeat(40))),
            ),
            (
                "/session_binding/published_session_id",
                Value::String(format!("agit-{}", "f".repeat(40))),
            ),
            (
                "/session_binding/source_commit",
                Value::String("e".repeat(40)),
            ),
            (
                "/session_binding/committer/account_id",
                Value::String("another-committer".into()),
            ),
            (
                "/session_binding/owner_key/account_id",
                Value::String("another-owner".into()),
            ),
            ("/session_binding/owner_key/key_version", Value::from(2)),
            (
                "/policy_digest",
                Value::String(digest_bytes(b"another policy")),
            ),
            (
                "/public_projection/content",
                Value::String("different public text".into()),
            ),
        ] {
            let mut changed = serde_json::to_value(&envelope).unwrap();
            *changed.pointer_mut(path).unwrap() = value;
            let mut changed: PrivacyEnvelope = serde_json::from_value(changed).unwrap();
            changed.private_payload.wrapped_keys[0].recipient = changed
                .session_binding
                .as_ref()
                .unwrap()
                .owner_key
                .recipient_id()
                .unwrap();
            changed.snapshot_digest = changed.public_digest().unwrap();
            changed.validate().unwrap();
            assert!(
                changed.open_content_key(&content_key).is_err(),
                "unauthenticated binding: {path}"
            );
        }
        let mut changed = envelope.clone();
        let key = SecretKey::from([7; 32]);
        changed.private_payload.wrapped_keys[0].ciphertext = STANDARD.encode(
            key.public_key()
                .seal(&mut OsRng, content_key.as_ref())
                .unwrap(),
        );
        changed.validate().unwrap();
        assert!(changed.open_content_key(&content_key).is_err());
        let mut downgraded = envelope.clone();
        downgraded.format_version = 1;
        assert!(downgraded.validate().is_err());
        downgraded.session_binding = None;
        assert!(downgraded.open_content_key(&content_key).is_err());
        let mut missing = envelope;
        missing.session_binding = None;
        assert!(missing.validate().is_err());
    }

    #[test]
    fn opens_browser_sealed_box_and_webcrypto_fixture() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/privacy-session-owner-browser-envelope.json"
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
        let response: ContentKeyResponse =
            serde_json::from_value(fixture["content_key_response"].clone()).unwrap();
        assert_eq!(
            STANDARD.encode(
                response
                    .open(
                        &response.snapshot,
                        fixture["envelope_bytes"].as_str().unwrap().as_bytes()
                    )
                    .unwrap()
            ),
            fixture["private_payload_base64"]
        );
    }
}
