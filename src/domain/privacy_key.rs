//! Repository viewing-key wrapping uses the Web's libsodium byte protocol.

use super::privacy_envelope::{ViewingRecipient, digest_bytes};
use anyhow::{Result, ensure};
use argon2::{Algorithm, Argon2, Block, Params, Version};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use crypto_secretbox::{
    Nonce, XSalsa20Poly1305,
    aead::{Aead, KeyInit},
};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const DEFAULT_OPSLIMIT: u64 = 3;
const DEFAULT_MEMLIMIT: u64 = 256 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncryptedPrivateKey {
    pub version: u32,
    pub algorithm: String,
    pub nonce: String,
    pub ciphertext: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KdfParams {
    pub algorithm: String,
    pub salt: String,
    pub opslimit: u64,
    pub memlimit: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyInput {
    pub public_key_algorithm: String,
    pub public_key: String,
    pub encrypted_private_key: EncryptedPrivateKey,
    pub kdf: KdfParams,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct KeyRecord {
    pub recipient: String,
    #[serde(default)]
    pub current: bool,
    #[serde(flatten)]
    pub key: KeyInput,
    pub updated_at: String,
}

pub fn recipient_id(public_key: &str) -> String {
    digest_bytes(public_key.as_bytes()).replace(':', "-")
}

pub fn validate_public_key(algorithm: &str, public_key: &str, recipient: &str) -> Result<()> {
    ensure!(
        algorithm == "x25519",
        "unsupported repository viewing-key algorithm"
    );
    decode_exact::<32>(public_key, "repository viewing public key")?;
    ensure!(
        recipient_id(public_key) == recipient,
        "repository viewing recipient does not match its public key"
    );
    ViewingRecipient::from_base64(recipient.to_owned(), public_key)?;
    Ok(())
}

impl KeyRecord {
    pub fn validate(&self) -> Result<()> {
        self.key.validate()?;
        validate_public_key(
            &self.key.public_key_algorithm,
            &self.key.public_key,
            &self.recipient,
        )
    }

    pub fn unlock(&self, password: &Zeroizing<String>) -> Result<Zeroizing<[u8; 32]>> {
        self.validate()?;
        self.key.unlock(password)
    }
}

impl KeyInput {
    pub fn validate(&self) -> Result<()> {
        validate_public_key(
            &self.public_key_algorithm,
            &self.public_key,
            &recipient_id(&self.public_key),
        )?;
        let wrapper = &self.encrypted_private_key;
        ensure!(
            wrapper.version == 2 && wrapper.algorithm == "xsalsa20-poly1305",
            "unsupported repository private-key wrapper"
        );
        decode_exact::<24>(&wrapper.nonce, "repository private-key nonce")?;
        decode_exact::<48>(&wrapper.ciphertext, "repository encrypted private key")?;
        self.kdf.validate()?;
        Ok(())
    }

    pub fn generate(password: &Zeroizing<String>) -> Result<Self> {
        let mut private_key = Zeroizing::new([0; 32]);
        OsRng.fill_bytes(private_key.as_mut());
        Self::wrap(&private_key, password)
    }

    pub fn wrap(private_key: &Zeroizing<[u8; 32]>, password: &Zeroizing<String>) -> Result<Self> {
        let mut salt = [0; 16];
        let mut nonce = [0; 24];
        OsRng.fill_bytes(&mut salt);
        OsRng.fill_bytes(&mut nonce);
        Self::wrap_with_params(
            private_key,
            password,
            KdfParams {
                algorithm: "argon2id13".into(),
                salt: STANDARD.encode(salt),
                opslimit: DEFAULT_OPSLIMIT,
                memlimit: DEFAULT_MEMLIMIT,
            },
            nonce,
        )
    }

    fn wrap_with_params(
        private_key: &Zeroizing<[u8; 32]>,
        password: &Zeroizing<String>,
        kdf: KdfParams,
        nonce: [u8; 24],
    ) -> Result<Self> {
        let derived = kdf.derive(password)?;
        let ciphertext = XSalsa20Poly1305::new_from_slice(derived.as_ref())
            .map_err(|_| anyhow::anyhow!("invalid repository wrapping key"))?
            .encrypt(Nonce::from_slice(&nonce), private_key.as_ref())
            .map_err(|_| anyhow::anyhow!("repository private-key wrapping failed"))?;
        let input = Self {
            public_key_algorithm: "x25519".into(),
            public_key: STANDARD.encode(x25519_dalek::x25519(
                **private_key,
                x25519_dalek::X25519_BASEPOINT_BYTES,
            )),
            encrypted_private_key: EncryptedPrivateKey {
                version: 2,
                algorithm: "xsalsa20-poly1305".into(),
                nonce: STANDARD.encode(nonce),
                ciphertext: STANDARD.encode(ciphertext),
            },
            kdf,
        };
        input.validate()?;
        Ok(input)
    }

    pub fn unlock(&self, password: &Zeroizing<String>) -> Result<Zeroizing<[u8; 32]>> {
        self.validate()?;
        let derived = self.kdf.derive(password)?;
        let nonce = decode_exact::<24>(
            &self.encrypted_private_key.nonce,
            "repository private-key nonce",
        )?;
        let ciphertext = decode_exact::<48>(
            &self.encrypted_private_key.ciphertext,
            "repository encrypted private key",
        )?;
        let plaintext = Zeroizing::new(
            XSalsa20Poly1305::new_from_slice(derived.as_ref())
                .map_err(|_| anyhow::anyhow!("invalid repository wrapping key"))?
                .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
                .map_err(|_| {
                    anyhow::anyhow!("incorrect repository password or damaged viewing-key record")
                })?,
        );
        let mut key = Zeroizing::new([0; 32]);
        ensure!(
            plaintext.len() == key.len(),
            "invalid repository viewing private key"
        );
        key.copy_from_slice(&plaintext);
        ensure!(
            STANDARD.encode(x25519_dalek::x25519(
                *key,
                x25519_dalek::X25519_BASEPOINT_BYTES
            )) == self.public_key,
            "repository viewing private key does not match its public key"
        );
        Ok(key)
    }
}

impl KdfParams {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.algorithm == "argon2id13",
            "unsupported repository password derivation algorithm"
        );
        ensure!(
            (1..=10).contains(&self.opslimit),
            "repository password operations limit is outside the supported range"
        );
        ensure!(
            (8 * 1024 * 1024..=1024 * 1024 * 1024).contains(&self.memlimit),
            "repository password memory limit is outside the supported range"
        );
        decode_exact::<16>(&self.salt, "repository password salt")?;
        Ok(())
    }

    fn derive(&self, password: &Zeroizing<String>) -> Result<Zeroizing<[u8; 32]>> {
        self.validate()?;
        ensure!(!password.is_empty(), "enter a repository viewing password");
        // Libsodium measures memory in bytes, truncates to KiB, and fixes parallelism to one.
        let params = Params::new(
            (self.memlimit / 1024) as u32,
            self.opslimit as u32,
            1,
            Some(32),
        )
        .map_err(|_| anyhow::anyhow!("unsupported repository password derivation parameters"))?;
        let salt = decode_exact::<16>(&self.salt, "repository password salt")?;
        let mut memory = Zeroizing::new(Vec::new());
        memory
            .try_reserve_exact(params.block_count())
            .map_err(|_| {
                anyhow::anyhow!("insufficient memory to derive repository password key")
            })?;
        memory.resize(params.block_count(), Block::default());
        let mut key = Zeroizing::new([0; 32]);
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into_with_memory(
                password.as_bytes(),
                &salt,
                key.as_mut(),
                memory.as_mut_slice(),
            )
            .map_err(|_| anyhow::anyhow!("repository password derivation failed"))?;
        Ok(key)
    }
}

fn decode_exact<const N: usize>(value: &str, label: &str) -> Result<[u8; N]> {
    ensure!(
        value.len() == N.div_ceil(3) * 4,
        "{label} has an invalid length"
    );
    let bytes = STANDARD
        .decode(value)
        .map_err(|_| anyhow::anyhow!("{label} is not canonical Base64"))?;
    ensure!(
        STANDARD.encode(&bytes) == value,
        "{label} is not canonical Base64"
    );
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("{label} has an invalid length"))
}

#[cfg(test)]
mod tests;
