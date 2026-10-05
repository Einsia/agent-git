//! A durable claim precedes native dispatch, so restart cannot authorize a duplicate.

use crate::protocol::Frame;
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::fs::File;

const MAX_RECEIPT: usize = 64 * 1024;

#[derive(Clone)]
pub(crate) struct Store {
    path: PathBuf,
    #[cfg(test)]
    gate: Option<std::sync::Arc<tokio::sync::Semaphore>>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Receipt {
    pub digest: Vec<u8>,
    pub response: Option<Frame>,
}

impl Store {
    pub fn at(path: PathBuf) -> Self {
        Self {
            path,
            #[cfg(test)]
            gate: None,
        }
    }

    #[cfg(test)]
    pub fn gated(path: PathBuf, gate: std::sync::Arc<tokio::sync::Semaphore>) -> Self {
        Self {
            path,
            gate: Some(gate),
        }
    }

    #[cfg(test)]
    async fn wait_gate(&self) {
        if let Some(gate) = &self.gate {
            gate.acquire().await.unwrap().forget();
        }
    }

    fn path(&self, key: &str) -> PathBuf {
        self.path
            .join(format!("{:x}.json", Sha256::digest(key.as_bytes())))
    }

    fn directory(&self) -> crate::Result<()> {
        crate::infra::config::create_state_dir(&self.path)?;
        ensure!(
            !std::fs::symlink_metadata(&self.path)?
                .file_type()
                .is_symlink(),
            "message receipt directory cannot be a symlink"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o700))?;
            File::open(
                self.path
                    .parent()
                    .context("receipt directory has no parent")?,
            )?
            .sync_all()?;
        }
        #[cfg(windows)]
        crate::infra::windows_security::private_directory(&self.path)?;
        Ok(())
    }

    fn read(path: &Path) -> crate::Result<Option<Receipt>> {
        let file = match crate::rc::native_inbox::open_regular(path) {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let mut bytes = Vec::new();
        file.take((MAX_RECEIPT + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_RECEIPT,
            "message receipt exceeds its size limit"
        );
        let receipt: Receipt = serde_json::from_slice(&bytes)?;
        ensure!(receipt.digest.len() == 32, "invalid message receipt digest");
        Ok(Some(receipt))
    }

    pub async fn lookup(&self, key: String) -> crate::Result<Option<Receipt>> {
        let path = self.path(&key);
        tokio::task::spawn_blocking(move || Self::read(&path)).await?
    }

    pub async fn claim(&self, key: String, digest: Vec<u8>) -> crate::Result<(Receipt, bool)> {
        #[cfg(test)]
        self.wait_gate().await;
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            store.directory()?;
            let path = store.path(&key);
            if let Some(receipt) = Self::read(&path)? {
                return Ok((receipt, false));
            }
            let receipt = Receipt {
                digest,
                response: None,
            };
            let mut file = tempfile::NamedTempFile::new_in(&store.path)?;
            file.write_all(&serde_json::to_vec(&receipt)?)?;
            file.as_file().sync_all()?;
            match file.persist_noclobber(&path) {
                Ok(_) => {
                    #[cfg(unix)]
                    File::open(&store.path)?.sync_all()?;
                    Ok((receipt, true))
                }
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok((
                    Self::read(&path)?.context("message claim disappeared")?,
                    false,
                )),
                Err(error) => Err(error.into()),
            }
        })
        .await?
    }

    pub async fn finish(&self, key: String, response: Option<Frame>) -> crate::Result<()> {
        #[cfg(test)]
        self.wait_gate().await;
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            store.directory()?;
            let path = store.path(&key);
            if let Some(response) = response {
                let mut receipt = Self::read(&path)?.context("message claim is missing")?;
                receipt.response = Some(response);
                let bytes = serde_json::to_vec(&receipt)?;
                ensure!(
                    bytes.len() <= MAX_RECEIPT,
                    "message receipt exceeds its size limit"
                );
                let mut file = tempfile::NamedTempFile::new_in(&store.path)?;
                file.write_all(&bytes)?;
                file.as_file().sync_all()?;
                file.persist(path)?;
            } else {
                std::fs::remove_file(path)?;
            }
            #[cfg(unix)]
            File::open(&store.path)?.sync_all()?;
            Ok(())
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::RequestId;

    #[tokio::test]
    async fn restart_retains_uncertainty_and_results_but_a_proven_refusal_allows_retry() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("receipts");
        let key = "principal/source/conversation/message".to_string();
        let digest = vec![7; 32];
        let store = Store::at(directory.clone());
        let (_, fresh) = store.claim(key.clone(), digest.clone()).await.unwrap();
        assert!(fresh);
        drop(store);
        let store = Store::at(directory);
        let (pending, fresh) = store.claim(key.clone(), vec![8; 32]).await.unwrap();
        assert!(!fresh);
        assert_eq!(pending.digest, digest);
        assert!(pending.response.is_none());
        let response = Frame::response(
            RequestId::fresh(),
            serde_json::json!({"turn_id":"accepted"}),
        );
        store.finish(key.clone(), Some(response)).await.unwrap();
        let (saved, fresh) = store.claim(key.clone(), digest.clone()).await.unwrap();
        assert!(!fresh);
        assert_eq!(
            saved.response.unwrap().result.unwrap()["turn_id"],
            "accepted"
        );
        store.finish(key.clone(), None).await.unwrap();
        assert!(store.claim(key, digest).await.unwrap().1);
    }
}
