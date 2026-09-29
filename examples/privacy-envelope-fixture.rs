//! Emits synthetic interop data; the test key is deliberately public and must not protect data.

use agit::domain::privacy_envelope::{PrivacyEnvelope, ViewingRecipient, digest_bytes};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use crypto_box::SecretKey;

fn main() -> anyhow::Result<()> {
    let secret = SecretKey::from([7; 32]);
    let recipient = ViewingRecipient::from_base64(
        "test-viewer".into(),
        &STANDARD.encode(secret.public_key().as_bytes()),
    )?;
    let private = b"synthetic original session, metadata and attachment bytes";
    let envelope = PrivacyEnvelope::seal(
        digest_bytes(b"synthetic policy"),
        digest_bytes(b"synthetic snapshot"),
        serde_json::json!({
            "logical_path": "<workspace>/src/main.rs",
            "runtime": "codex",
            "content": "safe synthetic session",
            "canonical_order": {"2": "two", "10": "ten", "\u{e000}": "bmp", "\u{1f600}": "astral"},
        }),
        private,
        &recipient,
        Vec::new(),
    )?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "test_only_viewing_private_key": STANDARD.encode(secret.to_bytes()),
            "viewing_public_key": STANDARD.encode(secret.public_key().as_bytes()),
            "private_payload_base64": STANDARD.encode(private),
            "aad_base64": STANDARD.encode(envelope.associated_data()?),
            "envelope": envelope,
        }))?
    );
    Ok(())
}
