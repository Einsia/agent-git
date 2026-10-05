use super::*;
use crate::rc::endpoint::receipts::Store;
use sha2::{Digest, Sha256};

pub(super) struct OpenClaim {
    pub id: String,
    key: String,
    digest: Vec<u8>,
    store: Store,
}

impl OpenClaim {
    pub fn prepare(
        p: &TerminalOpen,
        instance: &str,
        cwd: &std::path::Path,
    ) -> Result<Option<Self>, RpcError> {
        let (open_id, expected_instance) = match (&p.open_id, &p.instance_id) {
            (None, None) => return Ok(None),
            (Some(id), Some(instance)) if uuid::Uuid::parse_str(id).is_ok() => (id, instance),
            _ => {
                return Err(RpcError::new(
                    ErrorCode::MalformedFrame,
                    "terminal opening requires an operation UUID and daemon instance",
                ));
            }
        };
        if expected_instance != instance {
            return Err(Self::gone());
        }
        let key = serde_json::to_string(&(instance, &p.workspace_id, open_id)).unwrap();
        let id = format!("t-{:x}", Sha256::digest(key.as_bytes()));
        let digest = Sha256::digest(
            serde_json::to_vec(&(
                &p.project_id,
                cwd.as_os_str().as_encoded_bytes(),
                p.cols,
                p.rows,
            ))
            .unwrap(),
        )
        .to_vec();
        let store = Store::at(
            crate::rc::rc_dir()
                .map_err(Self::storage_error)?
                .join("terminal-open-receipts"),
        );
        Ok(Some(Self {
            id,
            key,
            digest,
            store,
        }))
    }

    pub async fn exists(&self) -> Result<bool, RpcError> {
        match self
            .store
            .lookup(self.key.clone())
            .await
            .map_err(Self::storage_error)?
        {
            None => Ok(false),
            Some(receipt) if receipt.digest == self.digest => Ok(true),
            Some(_) => Err(RpcError::new(
                ErrorCode::MalformedFrame,
                "terminal operation identity was reused with different parameters",
            )),
        }
    }

    pub async fn claim(&self) -> Result<(), RpcError> {
        let (_, fresh) = self
            .store
            .claim(self.key.clone(), self.digest.clone())
            .await
            .map_err(Self::storage_error)?;
        if !fresh {
            return Err(Self::gone());
        }
        Ok(())
    }

    pub async fn refuse(&self) -> Result<(), RpcError> {
        self.store
            .finish(self.key.clone(), None)
            .await
            .map_err(Self::storage_error)
    }

    pub fn gone() -> RpcError {
        RpcError::new(
            ErrorCode::SessionNotFound,
            "the terminal opening is no longer active",
        )
    }

    fn storage_error(error: anyhow::Error) -> RpcError {
        eprintln!("agitd: terminal operation receipt unavailable: {error}");
        RpcError::new(
            ErrorCode::Internal,
            "terminal operation receipt unavailable",
        )
    }
}
