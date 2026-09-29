//! Local records authenticate their scope before exposing private publication mappings.

use super::*;
use crate::domain::secret_filter::{KeyStore, RepositoryKeyStore};
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, OsRng, Payload, rand_core::RngCore},
};
use std::io::Read;
use zeroize::Zeroizing;

const KEY_ID: &str = "8233dc33-9794-416d-b1c6-9b4a490405df";
const MAX_BYTES: usize = 64 * 1024 * 1024;

pub(super) fn load(directory: &Path, scope: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(directory.join("index.enc")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot read authenticated privacy state"),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_BYTES as u64,
        "invalid authenticated privacy state carrier"
    );
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() >= 28 && bytes.len() <= MAX_BYTES,
        "invalid authenticated privacy state size"
    );
    let key = RepositoryKeyStore::new(directory.join("keys")).get(KEY_ID)?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|_| anyhow::anyhow!("invalid authenticated privacy state key"))?;
    cipher
        .decrypt(
            Nonce::from_slice(&bytes[..12]),
            Payload {
                msg: &bytes[12..],
                aad: scope.as_bytes(),
            },
        )
        .map(Zeroizing::new)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("privacy state authentication failed"))
}

pub(super) fn save(directory: &Path, scope: &str, plain: &[u8]) -> Result<()> {
    ensure!(
        plain.len() <= MAX_BYTES - 28,
        "authenticated privacy state exceeds its byte limit"
    );
    crate::infra::config::create_state_dir(directory)?;
    let keys = RepositoryKeyStore::new(directory.join("keys"));
    match fs::symlink_metadata(directory.join("keys").join(format!("{KEY_ID}.key"))) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut key = Zeroizing::new([0u8; 32]);
            OsRng.fill_bytes(key.as_mut());
            keys.set(KEY_ID, key.as_ref())?;
        }
        Err(error) => return Err(error.into()),
    }
    let key = keys.get(KEY_ID)?;
    let cipher = Aes256Gcm::new_from_slice(&key)
        .map_err(|_| anyhow::anyhow!("invalid authenticated privacy state key"))?;
    let mut nonce = [0; 12];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plain,
                aad: scope.as_bytes(),
            },
        )
        .map_err(|_| anyhow::anyhow!("cannot encrypt authenticated privacy state"))?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(&nonce)?;
    file.write_all(&ciphertext)?;
    file.as_file().sync_all()?;
    file.persist(directory.join("index.enc"))
        .map_err(|error| error.error)?;
    Ok(())
}
