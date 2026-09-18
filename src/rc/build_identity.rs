//! Immutable build and process identities for local daemon negotiation.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub const BUILD_ID: &str = env!("AGIT_BUILD_ID");
pub const RPC_FEATURES: &[&str] = &["peer-control-v1", "history-v2", "safe-restart-v1"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonIdentity {
    pub instance_id: String,
    pub build_id: String,
    pub executable: PathBuf,
    pub rpc_features: Vec<String>,
}

impl DaemonIdentity {
    pub fn current() -> std::io::Result<Self> {
        Ok(Self {
            instance_id: uuid::Uuid::new_v4().to_string(),
            build_id: BUILD_ID.into(),
            executable: std::env::current_exe()?,
            rpc_features: RPC_FEATURES
                .iter()
                .map(|feature| (*feature).into())
                .collect(),
        })
    }
}
