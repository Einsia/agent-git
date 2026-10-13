//! Emits synthetic interop data; the test key is deliberately public and must not protect data.

use agit::domain::privacy_envelope::{PrivacyEnvelope, ViewingRecipient, digest_bytes};
use agit::domain::session_privacy::{
    AcceptedSnapshot, AccountIdentity, CommitterAttribution, OwnerKeyBinding,
    SESSION_ENVELOPE_VERSION, SessionPrivacy, SessionPrivacyConfig, SessionSnapshotBinding,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use crypto_box::SecretKey;

fn main() -> anyhow::Result<()> {
    let secret = SecretKey::from([7; 32]);
    let owner_key = OwnerKeyBinding {
        account_id: "account-alice".into(),
        key_version: 1,
    };
    let recipient = ViewingRecipient::from_base64(
        owner_key.recipient_id()?,
        &STANDARD.encode(secret.public_key().as_bytes()),
    )?;
    let private = b"synthetic original session and metadata bytes";
    let config = SessionPrivacyConfig {
        repository_id: "00000000-0000-0000-0000-000000000001".into(),
        session_id: format!("agit-{}", "a".repeat(40)),
        privacy: SessionPrivacy::new(Some(AccountIdentity::new(
            "https://hub.test",
            "account-alice",
        )?))?,
        revision: 1,
        created_at: "2026-01-01T00:00:00Z".into(),
    };
    let binding = SessionSnapshotBinding {
        repository_id: config.repository_id.clone(),
        session_id: config.session_id.clone(),
        published_session_id: format!("agit-{}", "b".repeat(40)),
        owner_key,
        committer: CommitterAttribution::Account {
            account_id: "account-bob".into(),
        },
        source_commit: "c".repeat(40),
    };
    binding.verify_config(&config)?;
    let envelope = PrivacyEnvelope::seal_session(
        digest_bytes(b"synthetic policy"),
        serde_json::json!({
            "logical_path": "<workspace>/src/main.rs",
            "runtime": "codex",
            "content": "safe synthetic session",
            "canonical_order": {"2": "two", "10": "ten", "\u{e000}": "bmp", "\u{1f600}": "astral"},
        }),
        private,
        &recipient,
        binding.clone(),
    )?;
    let envelope_bytes = serde_json::to_string(&envelope)?;
    let accepted = AcceptedSnapshot {
        version: SESSION_ENVELOPE_VERSION,
        binding,
        published_commit: "d".repeat(40),
        envelope_digest: digest_bytes(envelope_bytes.as_bytes()),
    };
    let content_key = secret
        .unseal(&STANDARD.decode(&envelope.private_payload.wrapped_keys[0].ciphertext)?)
        .map_err(|_| anyhow::anyhow!("cannot unwrap synthetic fixture key"))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "test_only_viewing_private_key": STANDARD.encode(secret.to_bytes()),
            "viewing_public_key": STANDARD.encode(secret.public_key().as_bytes()),
            "private_payload_base64": STANDARD.encode(private),
            "aad_base64": STANDARD.encode(envelope.associated_data()?),
            "config": config,
            "content_key_response": {"snapshot": accepted, "content_key": STANDARD.encode(content_key)},
            "envelope_bytes": envelope_bytes,
            "envelope": envelope,
        }))?
    );
    Ok(())
}
