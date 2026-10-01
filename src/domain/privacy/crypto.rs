//! Authenticated dictionary envelopes bind ciphertext to its owner and immutable identity.

use aes_gcm::aead::{Aead, KeyInit, OsRng, Payload, rand_core::RngCore};
use aes_gcm::{Aes256Gcm, Nonce};
use anyhow::{Context, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub const MAX_PLAINTEXT_BYTES: usize = 256 * 1024;
pub const MAX_CIPHERTEXT_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub hub: String,
    pub account: String,
}

pub struct UserKey {
    pub owner: Owner,
    pub version: i64,
    bytes: Zeroizing<Vec<u8>>,
}

impl UserKey {
    pub fn new(owner: Owner, version: i64, bytes: Vec<u8>) -> crate::Result<Self> {
        let bytes = Zeroizing::new(bytes);
        ensure!(
            version > 0 && bytes.len() == 32,
            "invalid privacy key envelope"
        );
        Ok(Self {
            owner,
            version,
            bytes,
        })
    }

    pub(crate) fn encoded(&self) -> Zeroizing<String> {
        Zeroizing::new(STANDARD.encode(self.bytes.as_slice()))
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub format: u32,
    pub dictionary_id: String,
    pub key_version: i64,
    pub nonce: String,
    pub ciphertext: String,
}

fn associated_data(
    owner: &Owner,
    dictionary: &str,
    package: &str,
    version: i64,
) -> crate::Result<Vec<u8>> {
    uuid::Uuid::parse_str(dictionary).context("invalid dictionary identity")?;
    uuid::Uuid::parse_str(package).context("invalid package identity")?;
    Ok(serde_json::to_vec(&(
        "agentgit-private-dictionary",
        1u32,
        &owner.hub,
        &owner.account,
        dictionary,
        package,
        version,
    ))?)
}

pub fn seal(
    key: &UserKey,
    dictionary: &str,
    package: &str,
    plaintext: &[u8],
) -> crate::Result<Envelope> {
    ensure!(
        plaintext.len() <= MAX_PLAINTEXT_BYTES,
        "privacy package exceeds its byte budget"
    );
    let cipher = Aes256Gcm::new_from_slice(key.bytes.as_slice())
        .map_err(|_| anyhow::anyhow!("invalid privacy key"))?;
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let aad = associated_data(&key.owner, dictionary, package, key.version)?;
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| anyhow::anyhow!("privacy encryption failed"))?;
    Ok(Envelope {
        format: 1,
        dictionary_id: dictionary.to_owned(),
        key_version: key.version,
        nonce: STANDARD.encode(nonce),
        ciphertext: STANDARD.encode(ciphertext),
    })
}

pub fn open(
    key: &UserKey,
    package: &str,
    envelope: &Envelope,
) -> crate::Result<Zeroizing<Vec<u8>>> {
    ensure!(
        envelope.format == 1 && envelope.key_version == key.version,
        "unsupported privacy envelope"
    );
    ensure!(
        envelope.nonce.len() <= 32 && envelope.ciphertext.len() <= MAX_CIPHERTEXT_BYTES,
        "privacy envelope exceeds its byte budget"
    );
    let nonce = STANDARD
        .decode(&envelope.nonce)
        .context("invalid privacy nonce")?;
    ensure!(nonce.len() == 12, "invalid privacy nonce");
    let ciphertext = STANDARD
        .decode(&envelope.ciphertext)
        .context("invalid privacy ciphertext")?;
    let cipher = Aes256Gcm::new_from_slice(key.bytes.as_slice())
        .map_err(|_| anyhow::anyhow!("invalid privacy key"))?;
    let aad = associated_data(&key.owner, &envelope.dictionary_id, package, key.version)?;
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| anyhow::anyhow!("privacy package authentication failed"))?,
    );
    ensure!(
        plaintext.len() <= MAX_PLAINTEXT_BYTES,
        "privacy plaintext exceeds its byte budget"
    );
    Ok(plaintext)
}
